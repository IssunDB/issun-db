use crate::ast::*;
use crate::error::CypherError;
use chumsky::input::MappedInput;
use chumsky::pratt::{infix, left, prefix};
use chumsky::prelude::*;
use std::collections::HashMap;

// ─── Token ────────────────────────────────────────────────────────────────────

/// A Cypher token produced by the lexer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    // Literals
    Integer(i64),
    Float(f64),
    Str(String),
    Param(String), // $name
    Ident(String), // identifier or keyword (already upper-cased in keyword slot)

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

type ParserInput<'a> = MappedInput<'a, Tok, SimpleSpan, &'a [(Tok, SimpleSpan)]>;
type ParserError<'a> = extra::Err<Rich<'a, Tok>>;

/// Build an integer token from an unsigned magnitude string in the given radix.
/// Magnitudes up to `i64::MAX` map directly. A magnitude of exactly `2^63` maps
/// to `i64::MIN`, which is only valid when immediately negated (the unary-minus
/// folder accepts it; a standalone occurrence is rejected later in validation).
/// Anything larger overflows and yields `None` so the lexer can raise an error.
fn int_token(digits: &str, radix: u32) -> Option<Tok> {
    match u128::from_str_radix(digits, radix) {
        Ok(v) if v <= i64::MAX as u128 => Some(Tok::Integer(v as i64)),
        Ok(v) if v == i64::MAX as u128 + 1 => Some(Tok::Integer(i64::MIN)),
        _ => None,
    }
}

// ─── Phase 1: Spanned Lexer ───────────────────────────────────────────────────

/// Build a spanned chumsky lexer that converts a Cypher source string into a
/// sequence of `(Tok, SimpleSpan)` pairs to allow high-precision error highlighting.
pub(crate) fn lexer<'src>()
-> impl Parser<'src, &'src str, Vec<(Tok, SimpleSpan)>, extra::Err<Rich<'src, char>>> {
    // Hex integer: 0x1A or 0X1A
    let hex_int = just("0x")
        .or(just("0X"))
        .ignore_then(text::digits(16).to_slice())
        .validate(|s: &str, e, emitter| {
            int_token(s, 16).unwrap_or_else(|| {
                emitter.emit(Rich::custom(
                    e.span(),
                    "SyntaxError(IntegerOverflow): hexadecimal integer literal out of range",
                ));
                Tok::Integer(0)
            })
        });

    // Octal integer: 0o77 or 0O77
    let oct_int = just("0o")
        .or(just("0O"))
        .ignore_then(text::digits(8).to_slice())
        .validate(|s: &str, e, emitter| {
            int_token(s, 8).unwrap_or_else(|| {
                emitter.emit(Rich::custom(
                    e.span(),
                    "SyntaxError(IntegerOverflow): octal integer literal out of range",
                ));
                Tok::Integer(0)
            })
        });

    // Exponent for floats
    let exponent = choice((just('e'), just('E')))
        .then(just('-').or(just('+')).or_not())
        .then(text::digits(10));

    // A float literal that parses to infinity is out of range; raise an error.
    // (Inlined at both branches below because the `validate` closure's argument
    // types are inferred from the call site.)

    // Floating-point literals
    let float_num = choice((
        // 1.5 / 1.5e3
        text::int(10)
            .then(choice((
                just('.')
                    .then(text::digits(10))
                    .then(exponent.or_not())
                    .to_slice()
                    .map(Some),
                exponent.to_slice().map(Some),
            )))
            .to_slice()
            .validate(|s: &str, e, emitter| {
                let v: f64 = s.parse().unwrap_or(0.0);
                if v.is_infinite() {
                    emitter.emit(Rich::custom(
                        e.span(),
                        "SyntaxError(FloatingPointOverflow): floating point literal out of range",
                    ));
                }
                Tok::Float(v)
            }),
        // .5 / .5e-3
        just('.')
            .then(text::digits(10))
            .then(exponent.or_not())
            .to_slice()
            .validate(|s: &str, e, emitter| {
                let v: f64 = s.parse().unwrap_or(0.0);
                if v.is_infinite() {
                    emitter.emit(Rich::custom(
                        e.span(),
                        "SyntaxError(FloatingPointOverflow): floating point literal out of range",
                    ));
                }
                Tok::Float(v)
            }),
    ));

    // Plain integer literals
    let int_num = text::int(10).to_slice().validate(|s: &str, e, emitter| {
        int_token(s, 10).unwrap_or_else(|| {
            emitter.emit(Rich::custom(
                e.span(),
                "SyntaxError(IntegerOverflow): integer literal out of range",
            ));
            Tok::Integer(0)
        })
    });

    // A `\u` unicode escape requires exactly four hexadecimal digits. Consume up
    // to four hex digits after `u` and raise an error if fewer than four are
    // present, rather than silently falling back to a literal `u`.
    let unicode_escape = just('u')
        .ignore_then(
            any()
                .filter(|c: &char| c.is_ascii_hexdigit())
                .repeated()
                .at_most(4)
                .collect::<String>(),
        )
        .validate(|s: String, e, emitter| {
            if s.len() != 4 {
                emitter.emit(Rich::custom(
                    e.span(),
                    "SyntaxError(InvalidUnicodeLiteral): \\u requires four hexadecimal digits",
                ));
                return '\u{FFFD}';
            }
            u32::from_str_radix(&s, 16)
                .ok()
                .and_then(char::from_u32)
                .unwrap_or('\u{FFFD}')
        });

    // escape handler for strings
    let escape_sq = just('\\').ignore_then(choice((
        just('\'').to('\''),
        just('"').to('"'),
        just('\\').to('\\'),
        just('n').to('\n'),
        just('t').to('\t'),
        just('r').to('\r'),
        just('b').to('\x08'),
        just('f').to('\x0C'),
        unicode_escape,
        any().map(|c| c),
    )));

    let escape_dq = just('\\').ignore_then(choice((
        just('\'').to('\''),
        just('"').to('"'),
        just('\\').to('\\'),
        just('n').to('\n'),
        just('t').to('\t'),
        just('r').to('\r'),
        just('b').to('\x08'),
        just('f').to('\x0C'),
        unicode_escape,
        any().map(|c| c),
    )));

    // Single-quoted string literal
    let sq_str = just('\'')
        .ignore_then(
            choice((escape_sq, none_of("\\'")))
                .repeated()
                .collect::<String>(),
        )
        .then_ignore(just('\''))
        .map(Tok::Str);

    // Double-quoted string literal
    let dq_str = just('"')
        .ignore_then(
            choice((escape_dq, none_of("\\\"")))
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

    // Standard identifiers
    let ident = any()
        .filter(|c: &char| c.is_alphabetic() || *c == '_')
        .then(
            any()
                .filter(|c: &char| c.is_alphanumeric() || *c == '_')
                .repeated(),
        )
        .to_slice()
        .map(|s: &str| Tok::Ident(s.to_string()));

    // Multi-character symbols
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

    // Line comments
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
        .map_with(|tok, extra| (tok, extra.span()))
        .padded_by(comment.repeated())
        .padded()
        .repeated()
        .collect()
}

// ─── Helper Token Extractors ─────────────────────────────────────────────────

/// Matches specific symbolic tokens.
fn sym<'a>(expected: Tok) -> impl Parser<'a, ParserInput<'a>, (), ParserError<'a>> + Clone {
    any().filter(move |tok| *tok == expected).ignored()
}

/// Extracts standard identifiers.
fn identifier<'a>() -> impl Parser<'a, ParserInput<'a>, String, ParserError<'a>> + Clone {
    any().filter_map(|tok| match tok {
        Tok::Ident(name) => Some(name.clone()),
        _ => None,
    })
}

/// Matches specific keywords case-insensitively.
fn keyword<'a>(kw: &'static str) -> impl Parser<'a, ParserInput<'a>, (), ParserError<'a>> + Clone {
    any().filter_map(move |tok| match tok {
        Tok::Ident(name) if name.eq_ignore_ascii_case(kw) => Some(()),
        _ => None,
    })
}

// ─── Primitive Literal Parsers ───────────────────────────────────────────────

/// Parses basic Cypher literal values.
fn literal<'a>() -> impl Parser<'a, ParserInput<'a>, Literal, ParserError<'a>> + Clone {
    let int_lit = any().filter_map(|tok| match tok {
        Tok::Integer(n) => Some(Literal::Int(n)),
        _ => None,
    });

    let float_lit = any().filter_map(|tok| match tok {
        Tok::Float(f) => Some(Literal::Float(f)),
        _ => None,
    });

    let str_lit = any().filter_map(|tok| match tok {
        Tok::Str(s) => Some(Literal::Str(s.clone())),
        _ => None,
    });

    let bool_lit = choice((
        keyword("TRUE").to(Literal::Bool(true)),
        keyword("FALSE").to(Literal::Bool(false)),
    ));

    let null_lit = keyword("NULL").to(Literal::Null);

    choice((int_lit, float_lit, str_lit, bool_lit, null_lit))
}

// ─── Phase 2: Expression Pratt Parser ─────────────────────────────────────────

pub(crate) fn expr_parser<'a>() -> impl Parser<'a, ParserInput<'a>, Expr, ParserError<'a>> + Clone {
    #[derive(Clone)]
    enum SubscriptOrSlice {
        Subscript(Expr),
        Slice {
            start: Option<Expr>,
            end: Option<Expr>,
        },
    }

    #[derive(Clone)]
    enum PostfixOp {
        Dot(String),
        Subscript(Expr),
        Slice {
            start: Option<Expr>,
            end: Option<Expr>,
        },
        IsNull,
        IsNotNull,
        /// `n:Label` label predicate in a WHERE expression.
        LabelCheck(String),
    }

    recursive(|expr| {
        // --- Core Atomic Elements ---
        // A magnitude of exactly 2^63 lexes to `i64::MIN`. It is only valid when
        // immediately negated (yielding `i64::MIN`); the unary-minus folder
        // rewrites this marker back to the literal. A surviving marker is a
        // standalone out-of-range positive literal, rejected during validation.
        let lit_expr = literal().map(|lit| match lit {
            Literal::Int(n) if n == i64::MIN => Expr::FunctionCall {
                name: "__neg_min_int__".to_string(),
                args: vec![],
            },
            other => Expr::Literal(other),
        });

        let param_expr = any().filter_map(|tok| match tok {
            Tok::Param(p) => Some(Expr::Param(p.clone())),
            _ => None,
        });

        // Bare identifier is initially mapped to Prop(name, "") as per legacy parser design
        let var_expr = identifier().map(|name| Expr::Prop(name, "".to_string()));

        // Parentheses make the enclosed expression a complete, self-contained operand.
        // When that operand is a comparison, mark it with the internal `__grouped__`
        // wrapper so the executor's chained-comparison desugaring (which fires on the
        // bare `Cmp(Cmp(..), ..)` shape) does not flatten it: `(a = b) = c` must compare
        // the boolean result of `a = b` to `c`, not become `a = b AND b = c`. The wrapper
        // is transparent to every other consumer because they all recurse into
        // `FunctionCall` arguments.
        let paren_expr = sym(Tok::LParen)
            .ignore_then(expr.clone())
            .then_ignore(sym(Tok::RParen))
            .map(|inner| match &inner {
                Expr::BinaryOp { op, .. } if is_comparison_operator(op) => Expr::FunctionCall {
                    name: "__grouped__".to_string(),
                    args: vec![inner],
                },
                _ => inner,
            });

        // count(*) special case
        let count_star = keyword("COUNT")
            .ignore_then(sym(Tok::LParen))
            .ignore_then(sym(Tok::Star))
            .ignore_then(sym(Tok::RParen))
            .to(Expr::CountStar);

        // Inline property maps: { key: val, ... }
        let map_expr = sym(Tok::LBrace)
            .ignore_then(
                identifier()
                    .then_ignore(sym(Tok::Colon))
                    .then(expr.clone())
                    .separated_by(sym(Tok::Comma))
                    .allow_trailing()
                    .collect::<Vec<(String, Expr)>>(),
            )
            .then_ignore(sym(Tok::RBrace))
            .map(|pairs| {
                let mut args = Vec::new();
                for (key, val) in pairs {
                    args.push(Expr::Literal(Literal::Str(key)));
                    args.push(val);
                }
                Expr::FunctionCall {
                    name: "__map__".to_string(),
                    args,
                }
            });

        // List literal or List comprehension: [ ... ]
        let list_expr = sym(Tok::LBrack).ignore_then(choice((
            // Pattern comprehension: [pathvar = (a)-[r]->(b) WHERE predicate | transform].
            // The pattern always contains at least one relationship, so its leading
            // `(` (or `pathvar =`) distinguishes it from a list comprehension (which
            // begins with `identifier IN`) or a plain list literal. The transform after
            // `|` is mandatory.
            pattern(expr.clone())
                .then(keyword("WHERE").ignore_then(expr.clone()).or_not())
                .then_ignore(sym(Tok::Pipe))
                .then(expr.clone())
                .then_ignore(sym(Tok::RBrack))
                .map(|((pat, predicate), transform)| Expr::PatternComprehension {
                    pattern: Box::new(pat),
                    predicate: predicate.map(Box::new),
                    transform: Box::new(transform),
                }),
            // List comprehension: [x IN list WHERE predicate | transform]
            identifier()
                .then_ignore(keyword("IN"))
                .then(expr.clone())
                .then(keyword("WHERE").ignore_then(expr.clone()).or_not())
                .then(sym(Tok::Pipe).ignore_then(expr.clone()).or_not())
                .then_ignore(sym(Tok::RBrack))
                .map(
                    |(((var, list), predicate), transform)| Expr::ListComprehension {
                        variable: var,
                        list: Box::new(list),
                        predicate: predicate.map(Box::new),
                        transform: transform.map(Box::new),
                    },
                ),
            // Plain list literal: [item1, item2, ...]
            expr.clone()
                .separated_by(sym(Tok::Comma))
                .allow_trailing()
                .collect::<Vec<Expr>>()
                .then_ignore(sym(Tok::RBrack))
                .map(|items| Expr::FunctionCall {
                    name: "__list__".to_string(),
                    args: items,
                }),
        )));

        // CASE expression: CASE [subject] WHEN cond THEN result ... [ELSE default] END
        let case_expr = keyword("CASE")
            .ignore_then(keyword("WHEN").not().ignore_then(expr.clone()).or_not())
            .then(
                keyword("WHEN")
                    .ignore_then(expr.clone())
                    .then_ignore(keyword("THEN"))
                    .then(expr.clone())
                    .map(|(when, then)| CaseArm { when, then })
                    .repeated()
                    .at_least(1)
                    .collect(),
            )
            .then(keyword("ELSE").ignore_then(expr.clone()).or_not())
            .then_ignore(keyword("END"))
            .map(|((subject, arms), else_expr)| Expr::Case {
                subject: subject.map(Box::new),
                arms,
                else_expr: else_expr.map(Box::new),
            });

        // Quantifier expressions: ALL, ANY, NONE, SINGLE
        let quantifier_kind = choice((
            keyword("ALL").to(QuantifierKind::All),
            keyword("ANY").to(QuantifierKind::Any),
            keyword("NONE").to(QuantifierKind::None),
            keyword("SINGLE").to(QuantifierKind::Single),
        ));

        let quantifier_expr = quantifier_kind
            .then_ignore(sym(Tok::LParen))
            .then(identifier())
            .then_ignore(keyword("IN"))
            .then(expr.clone())
            .then_ignore(keyword("WHERE"))
            .then(expr.clone())
            .then_ignore(sym(Tok::RParen))
            .map(|(((kind, variable), list), predicate)| Expr::Quantifier {
                kind,
                variable,
                list: Box::new(list),
                predicate: Box::new(predicate),
            });

        // Standard function calls & Aggregations (e.g. count(distinct x), sum(x), percentileDisc(0.95))
        let distinct_flag = keyword("DISTINCT")
            .to(true)
            .or_not()
            .map(|d| d.unwrap_or(false));

        let agg_fn_type = choice((
            keyword("COUNT").to(AggFn::Count { distinct: false }),
            keyword("SUM").to(AggFn::Sum { distinct: false }),
            keyword("AVG").to(AggFn::Avg { distinct: false }),
            keyword("MIN").to(AggFn::Min { distinct: false }),
            keyword("MAX").to(AggFn::Max { distinct: false }),
            keyword("COLLECT").to(AggFn::Collect { distinct: false }),
            keyword("STDEV").to(AggFn::StDev { distinct: false }),
            keyword("STDEVP").to(AggFn::StDevP { distinct: false }),
        ));

        // Normal aggregation: COUNT(DISTINCT x)
        let normal_agg = agg_fn_type
            .then_ignore(sym(Tok::LParen))
            .then(distinct_flag)
            .then(expr.clone())
            .then_ignore(sym(Tok::RParen))
            .map(|((fn_type, distinct), inner)| {
                let fn_type = match fn_type {
                    AggFn::Count { .. } => AggFn::Count { distinct },
                    AggFn::Sum { .. } => AggFn::Sum { distinct },
                    AggFn::Avg { .. } => AggFn::Avg { distinct },
                    AggFn::Min { .. } => AggFn::Min { distinct },
                    AggFn::Max { .. } => AggFn::Max { distinct },
                    AggFn::Collect { .. } => AggFn::Collect { distinct },
                    AggFn::StDev { .. } => AggFn::StDev { distinct },
                    AggFn::StDevP { .. } => AggFn::StDevP { distinct },
                    other => other,
                };
                Expr::Agg(fn_type, Box::new(inner))
            });

        // Percentile aggregation: percentileDisc(x, percentile)
        let percentile_agg = choice((
            keyword("PERCENTILEDISC").to(true),
            keyword("PERCENTILECONT").to(false),
        ))
        .then_ignore(sym(Tok::LParen))
        .then(expr.clone())
        .then_ignore(sym(Tok::Comma))
        .then(expr.clone())
        .then_ignore(sym(Tok::RParen))
        .map(|((is_disc, inner), percentile)| {
            let percentile = Box::new(percentile);
            let fn_type = if is_disc {
                AggFn::PercentileDisc { percentile }
            } else {
                AggFn::PercentileCont { percentile }
            };
            Expr::Agg(fn_type, Box::new(inner))
        });

        // Standard scalar function call: range(1, 10)
        let standard_fn_call = identifier()
            .then_ignore(sym(Tok::LParen))
            .then(
                expr.clone()
                    .separated_by(sym(Tok::Comma))
                    .allow_trailing()
                    .collect(),
            )
            .then_ignore(sym(Tok::RParen))
            .map(|(name, args)| Expr::FunctionCall { name, args });

        // Namespace-qualified function calls: date.truncate(...), duration.between(...), etc.
        let dotted_fn_call = identifier()
            .then_ignore(sym(Tok::Dot))
            .then(identifier())
            .then_ignore(sym(Tok::LParen))
            .then(
                expr.clone()
                    .separated_by(sym(Tok::Comma))
                    .allow_trailing()
                    .collect::<Vec<Expr>>(),
            )
            .then_ignore(sym(Tok::RParen))
            .map(|((namespace, func), args)| Expr::FunctionCall {
                name: format!("{}.{}", namespace, func),
                args,
            });

        // filter(var IN list WHERE predicate) -> list comprehension without transform
        let filter_fn = keyword("FILTER")
            .ignore_then(sym(Tok::LParen))
            .ignore_then(identifier())
            .then_ignore(keyword("IN"))
            .then(expr.clone())
            .then_ignore(keyword("WHERE"))
            .then(expr.clone())
            .then_ignore(sym(Tok::RParen))
            .map(|((var, list), predicate)| Expr::ListComprehension {
                variable: var,
                list: Box::new(list),
                predicate: Some(Box::new(predicate)),
                transform: None,
            });

        // extract(var IN list WHERE predicate | transform)
        let extract_fn = keyword("EXTRACT")
            .ignore_then(sym(Tok::LParen))
            .ignore_then(identifier())
            .then_ignore(keyword("IN"))
            .then(expr.clone())
            .then(keyword("WHERE").ignore_then(expr.clone()).or_not())
            .then(sym(Tok::Pipe).ignore_then(expr.clone()).or_not())
            .then_ignore(sym(Tok::RParen))
            .map(
                |(((var, list), predicate), transform)| Expr::ListComprehension {
                    variable: var,
                    list: Box::new(list),
                    predicate: predicate.map(Box::new),
                    transform: transform.map(Box::new),
                },
            );

        // reduce(acc = initial, var IN list | expression)
        let reduce_fn = keyword("REDUCE")
            .ignore_then(sym(Tok::LParen))
            .ignore_then(identifier())
            .then_ignore(sym(Tok::Eq))
            .then(expr.clone())
            .then_ignore(sym(Tok::Comma))
            .then(identifier())
            .then_ignore(keyword("IN"))
            .then(expr.clone())
            .then_ignore(sym(Tok::Pipe))
            .then(expr.clone())
            .then_ignore(sym(Tok::RParen))
            .map(|((((acc, initial), var), list), expression)| Expr::Reduce {
                accumulator: acc,
                initial: Box::new(initial),
                variable: var,
                list: Box::new(list),
                expression: Box::new(expression),
            });

        let atom_choices = choice((
            count_star,
            quantifier_expr,
            case_expr,
            list_expr,
            map_expr,
            normal_agg,
            percentile_agg,
            filter_fn,
            extract_fn,
            reduce_fn,
            dotted_fn_call,
            standard_fn_call,
            lit_expr,
            param_expr,
            var_expr,
            paren_expr,
        ));

        // Postfix operations (chained left-associatively):
        let postfix = atom_choices.foldl(
            choice((
                // .property
                sym(Tok::Dot).ignore_then(identifier()).map(PostfixOp::Dot),
                // [index] or [start..end]
                sym(Tok::LBrack)
                    .ignore_then(choice((
                        // [..end]
                        sym(Tok::DotDot)
                            .ignore_then(expr.clone().or_not())
                            .map(|end| SubscriptOrSlice::Slice { start: None, end }),
                        // [start..] or [start] or [start..end]
                        expr.clone()
                            .then(choice((
                                sym(Tok::DotDot)
                                    .ignore_then(expr.clone().or_not())
                                    .map(Some),
                                any().rewind().to(None),
                            )))
                            .map(|(start, opt_end)| match opt_end {
                                Some(end) => SubscriptOrSlice::Slice {
                                    start: Some(start),
                                    end,
                                },
                                None => SubscriptOrSlice::Subscript(start),
                            }),
                    )))
                    .then_ignore(sym(Tok::RBrack))
                    .map(|sub_or_slice| match sub_or_slice {
                        SubscriptOrSlice::Slice { start, end } => PostfixOp::Slice { start, end },
                        SubscriptOrSlice::Subscript(idx) => PostfixOp::Subscript(idx),
                    }),
                // IS NULL / IS NOT NULL
                keyword("IS").ignore_then(choice((
                    keyword("NOT")
                        .ignore_then(keyword("NULL"))
                        .to(PostfixOp::IsNotNull),
                    keyword("NULL").to(PostfixOp::IsNull),
                ))),
                // n:Label: label predicate usable in WHERE expressions.

                // Only applies when the left operand is a bare identifier (Prop(name, "")).
                sym(Tok::Colon)
                    .ignore_then(identifier())
                    .map(PostfixOp::LabelCheck),
            ))
            .repeated(),
            |expr, op| match op {
                PostfixOp::LabelCheck(label) => {
                    // Extract the variable name from a bare identifier expression.
                    let variable = match &expr {
                        Expr::Prop(var, empty) if empty.is_empty() => var.clone(),
                        _ => return expr, // non-identifier: ignore the colon (shouldn't happen)
                    };
                    Expr::HasLabel { variable, label }
                }
                PostfixOp::Dot(prop) => match expr {
                    Expr::Prop(var, ref empty) if empty.is_empty() => {
                        Expr::Prop(var.clone(), prop.clone())
                    }
                    other => Expr::Subscript {
                        expr: Box::new(other),
                        index: Box::new(Expr::Literal(Literal::Str(prop.clone()))),
                    },
                },
                PostfixOp::Subscript(idx) => Expr::Subscript {
                    expr: Box::new(expr),
                    index: Box::new(idx),
                },
                PostfixOp::Slice { start, end } => Expr::Slice {
                    expr: Box::new(expr),
                    start: start.map(Box::new),
                    end: end.map(Box::new),
                },
                PostfixOp::IsNull => Expr::IsNull(Box::new(expr)),
                PostfixOp::IsNotNull => Expr::IsNotNull(Box::new(expr)),
            },
        );

        // Pratt precedence rules for binary and unary operations.
        //
        // openCypher precedence order (tightest to loosest binding):
        //   postfix (., []) > ^ > unary- > *, /, % > +, - >
        //   comparisons/IN/CONTAINS/STARTS WITH/ENDS WITH > NOT > AND > XOR > OR
        //
        // Levels (higher number = tighter binding):
        //   OR=10, XOR=11, AND=12, NOT(prefix)=12, comparisons+IN+etc=13,
        //   +/-=14, */%=15, unary-=15, ^=16
        //
        // For prefix(P): the right operand is parsed consuming ops with bp > P.
        // prefix(12) for NOT: AND(12) is NOT consumed (12 > 12 false), comparison(13)
        // IS consumed (13 > 12 true), giving the correct NOT > AND but NOT < comparisons.
        // openCypher Pratt precedence table (all levels; higher = tighter binding).
        // Empirically chumsky's prefix(P) consumes infix(left(P)) (same level), so
        // NOT must be at prefix(13); one above AND(12); to give (NOT a) AND b.

        //
        //  10: OR         11: XOR       12: AND
        //  13: NOT (prefix; grabs comparisons 14 but not AND 12)

        //  14: =,<>,<,>,<=,>=,=~
        //  15: IN, CONTAINS, STARTS WITH, ENDS WITH  (tighter than =)
        //  16: +, -  (binary)
        //  17: *, /, %
        //  18: ^  (left-assoc)
        //  19: unary -  (prefix; binds tighter than ^, so -3^2 = (-3)^2 = 9)

        let pratt = postfix.pratt((
            // Unary minus at 19, one above ^ (18): chumsky's prefix(P) lets infix(left(P))
            // bind into its operand, so unary minus must sit ABOVE ^ for openCypher's
            // "numeric unary negative takes precedence over exponentiation" (-3^2 = 9).
            // Fold negation into integer/float literals at parse time so that:
            //   -1 becomes Literal(Int(-1)) rather than BinaryOp(Sub, 0, 1)
            //   -1.5 becomes Literal(Float(-1.5))
            // This also fixes the column display ("abs(-1)" instead of "abs(0 - 1)")
            // and correctly handles i64::MIN (-9223372036854775808).
            prefix(19, sym(Tok::Minus), |_, x, _| match x {
                // Negating the 2^63 marker yields i64::MIN, the smallest integer.
                Expr::FunctionCall { name, .. } if name == "__neg_min_int__" => {
                    Expr::Literal(Literal::Int(i64::MIN))
                }
                Expr::Literal(Literal::Int(n)) => Expr::Literal(Literal::Int(n.wrapping_neg())),
                Expr::Literal(Literal::Float(f)) => Expr::Literal(Literal::Float(-f)),
                other => Expr::BinaryOp {
                    op: BinaryOperator::Sub,
                    left: Box::new(Expr::Literal(Literal::Int(0))),
                    right: Box::new(other),
                },
            }),
            // Power: left-associative.
            infix(left(18), sym(Tok::Caret), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Pow,
                left: Box::new(l),
                right: Box::new(r),
            }),
            // Multiplicative.
            infix(left(17), sym(Tok::Star), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Mul,
                left: Box::new(l),
                right: Box::new(r),
            }),
            infix(left(17), sym(Tok::Slash), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Div,
                left: Box::new(l),
                right: Box::new(r),
            }),
            infix(left(17), sym(Tok::Percent), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Mod,
                left: Box::new(l),
                right: Box::new(r),
            }),
            // Additive.
            infix(left(16), sym(Tok::Plus), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Add,
                left: Box::new(l),
                right: Box::new(r),
            }),
            infix(left(16), sym(Tok::Minus), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Sub,
                left: Box::new(l),
                right: Box::new(r),
            }),
            // Comparisons at 14; IN/CONTAINS/STARTS_WITH/ENDS_WITH at 15 (tighter).
            infix(left(14), sym(Tok::Eq), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Eq,
                left: Box::new(l),
                right: Box::new(r),
            }),
            infix(left(14), sym(Tok::RegexEq), |l, _, r, _| {
                Expr::FunctionCall {
                    name: "__regex__".to_string(),
                    args: vec![l, r],
                }
            }),
            infix(left(14), sym(Tok::Ne), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Ne,
                left: Box::new(l),
                right: Box::new(r),
            }),
            infix(left(14), sym(Tok::Lt), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Lt,
                left: Box::new(l),
                right: Box::new(r),
            }),
            infix(left(14), sym(Tok::Gt), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Gt,
                left: Box::new(l),
                right: Box::new(r),
            }),
            infix(left(14), sym(Tok::Le), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Le,
                left: Box::new(l),
                right: Box::new(r),
            }),
            infix(left(14), sym(Tok::Ge), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Ge,
                left: Box::new(l),
                right: Box::new(r),
            }),
            // Membership and string-matching operators (level 15, tighter than comparisons 14).
            // x IN list
            infix(left(15), keyword("IN"), |l, _, r, _| Expr::FunctionCall {
                name: "__in__".to_string(),
                args: vec![l, r],
            }),
            // x NOT IN list
            infix(
                left(15),
                keyword("NOT").then_ignore(keyword("IN")),
                |l, _, r, _| {
                    Expr::Not(Box::new(Expr::FunctionCall {
                        name: "__in__".to_string(),
                        args: vec![l, r],
                    }))
                },
            ),
            // x CONTAINS y
            infix(left(15), keyword("CONTAINS"), |l, _, r, _| {
                Expr::FunctionCall {
                    name: "__contains__".to_string(),
                    args: vec![l, r],
                }
            }),
            // x NOT CONTAINS y
            infix(
                left(15),
                keyword("NOT").then_ignore(keyword("CONTAINS")),
                |l, _, r, _| {
                    Expr::Not(Box::new(Expr::FunctionCall {
                        name: "__contains__".to_string(),
                        args: vec![l, r],
                    }))
                },
            ),
            // x STARTS WITH y
            infix(
                left(15),
                keyword("STARTS").then_ignore(keyword("WITH")),
                |l, _, r, _| Expr::FunctionCall {
                    name: "__starts_with__".to_string(),
                    args: vec![l, r],
                },
            ),
            // x NOT STARTS WITH y
            infix(
                left(15),
                keyword("NOT")
                    .then_ignore(keyword("STARTS"))
                    .then_ignore(keyword("WITH")),
                |l, _, r, _| {
                    Expr::Not(Box::new(Expr::FunctionCall {
                        name: "__starts_with__".to_string(),
                        args: vec![l, r],
                    }))
                },
            ),
            // x ENDS WITH y
            infix(
                left(15),
                keyword("ENDS").then_ignore(keyword("WITH")),
                |l, _, r, _| Expr::FunctionCall {
                    name: "__ends_with__".to_string(),
                    args: vec![l, r],
                },
            ),
            // x NOT ENDS WITH y
            infix(
                left(15),
                keyword("NOT")
                    .then_ignore(keyword("ENDS"))
                    .then_ignore(keyword("WITH")),
                |l, _, r, _| {
                    Expr::Not(Box::new(Expr::FunctionCall {
                        name: "__ends_with__".to_string(),
                        args: vec![l, r],
                    }))
                },
            ),
            // NOT: prefix(13) so it does not consume AND(12) but does consume comparisons(14).
            prefix(13, keyword("NOT"), |_, x, _| Expr::Not(Box::new(x))),
            // AND
            infix(left(12), keyword("AND"), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::And,
                left: Box::new(l),
                right: Box::new(r),
            }),
            // XOR
            infix(left(11), keyword("XOR"), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Xor,
                left: Box::new(l),
                right: Box::new(r),
            }),
            // OR (lowest-precedence logical operator)
            infix(left(10), keyword("OR"), |l, _, r, _| Expr::BinaryOp {
                op: BinaryOperator::Or,
                left: Box::new(l),
                right: Box::new(r),
            }),
        ));

        pratt
    })
}

// ─── Phase 3: Structural Graph Patterns ───────────────────────────────────────

/// Parses inline property maps `{ key: expr, ... }`
fn property_map<'a>(
    expr_parser: impl Parser<'a, ParserInput<'a>, Expr, ParserError<'a>> + Clone + 'a,
) -> impl Parser<'a, ParserInput<'a>, HashMap<String, Expr>, ParserError<'a>> + Clone {
    sym(Tok::LBrace)
        .ignore_then(
            identifier()
                .then_ignore(sym(Tok::Colon))
                .then(expr_parser)
                .separated_by(sym(Tok::Comma))
                .allow_trailing()
                .collect::<HashMap<String, Expr>>(),
        )
        .then_ignore(sym(Tok::RBrace))
}

/// Parses node patterns: `(variable:Label { properties })` or `(n:A:B:C { ... })`.
///
/// Multiple labels are allowed in Cypher (e.g., `(n:Person:Employee)`).  Only the
/// every `:Label` segment is stored in `NodePattern::labels` in source order.
fn node_pattern<'a>(
    expr_parser: impl Parser<'a, ParserInput<'a>, Expr, ParserError<'a>> + Clone + 'a,
) -> impl Parser<'a, ParserInput<'a>, NodePattern, ParserError<'a>> + Clone {
    sym(Tok::LParen)
        .ignore_then(
            identifier()
                .or_not()
                .then(
                    // Accept one or more `:Label` segments; all are retained.
                    sym(Tok::Colon)
                        .ignore_then(identifier())
                        .repeated()
                        .collect::<Vec<String>>(),
                )
                .then(property_map(expr_parser).or_not()),
        )
        .then_ignore(sym(Tok::RParen))
        .map(|((variable, labels), properties)| NodePattern {
            variable,
            labels,
            properties,
        })
}

/// Parses edge hop ranges: `*1..3` or `*..5` or `*2` or bare `*`
fn rel_range<'a>() -> impl Parser<'a, ParserInput<'a>, RelRange, ParserError<'a>> + Clone {
    let int_u32 = any().filter_map(|tok| match tok {
        Tok::Integer(n) => Some(n as u32),
        _ => None,
    });

    sym(Tok::Star).ignore_then(choice((
        // *start..end or *start..
        int_u32
            .then(sym(Tok::DotDot).ignore_then(int_u32.or_not()).or_not())
            .map(|(start, opt_range)| match opt_range {
                Some(end) => RelRange {
                    min: Some(start).or(Some(1)),
                    max: end,
                },
                None => RelRange {
                    min: Some(start),
                    max: Some(start),
                },
            }),
        // *..end or *..
        sym(Tok::DotDot)
            .ignore_then(int_u32.or_not())
            .map(|end| RelRange {
                min: Some(1),
                max: end,
            }),
        // Bare *
        any().rewind().to(RelRange {
            min: Some(1),
            max: None,
        }),
    )))
}

/// Parses directional or undirected relationship patterns
fn relationship_pattern<'a>(
    expr_parser: impl Parser<'a, ParserInput<'a>, Expr, ParserError<'a>> + Clone + 'a,
) -> impl Parser<'a, ParserInput<'a>, RelationshipPattern, ParserError<'a>> + Clone {
    let prefix = choice((
        sym(Tok::LArrow).to((true, false)), // inbound: <-
        sym(Tok::Minus).to((false, false)), // outbound/undirected: -
    ));

    // Bare arrow edge patterns (with no brackets)
    let bare_inbound = sym(Tok::LArrow)
        .then_ignore(sym(Tok::Minus))
        .map(|_| RelationshipPattern {
            variable: None,
            rel_type: None,
            is_incoming: true,
            is_undirected: false,
            range: None,
            properties: None,
        });

    let bare_other = sym(Tok::Minus).ignore_then(choice((
        sym(Tok::Arrow).to(RelationshipPattern {
            variable: None,
            rel_type: None,
            is_incoming: false,
            is_undirected: false,
            range: None,
            properties: None,
        }),
        sym(Tok::Minus).to(RelationshipPattern {
            variable: None,
            rel_type: None,
            is_incoming: false,
            is_undirected: true,
            range: None,
            properties: None,
        }),
    )));

    // Standard bracketed relationship patterns: -[variable:Type*range { props }]->
    let bracketed = prefix
        .then(
            sym(Tok::LBrack)
                .ignore_then(
                    identifier()
                        .or_not()
                        .then(
                            sym(Tok::Colon)
                                .ignore_then(
                                    identifier()
                                        .then(
                                            sym(Tok::Pipe)
                                                .ignore_then(sym(Tok::Colon).or_not())
                                                .ignore_then(identifier())
                                                .repeated()
                                                .collect::<Vec<String>>(),
                                        )
                                        .map(|(first, rest)| {
                                            let mut s = first;
                                            for r in rest {
                                                s.push('|');
                                                s.push_str(&r);
                                            }
                                            s
                                        }),
                                )
                                .or_not(),
                        )
                        .then(rel_range().or_not())
                        .then(property_map(expr_parser).or_not()),
                )
                .then_ignore(sym(Tok::RBrack))
                .then(choice((
                    sym(Tok::Arrow).to(false), // -> (is_undirected = false)
                    sym(Tok::Minus).to(true),  // -  (is_undirected = true)
                ))),
        )
        .map(
            |((is_incoming, _), ((((variable, rel_type), range), properties), is_minus_suffix))| {
                let is_undirected = !is_incoming && is_minus_suffix;
                RelationshipPattern {
                    variable,
                    rel_type,
                    is_incoming,
                    is_undirected,
                    range,
                    properties,
                }
            },
        );

    choice((bare_inbound, bare_other, bracketed))
}

/// Parses path patterns
fn pattern<'a>(
    expr_parser: impl Parser<'a, ParserInput<'a>, Expr, ParserError<'a>> + Clone + 'a,
) -> impl Parser<'a, ParserInput<'a>, Pattern, ParserError<'a>> + Clone {
    let path_prefix = identifier().then_ignore(sym(Tok::Eq)).or_not();

    path_prefix
        .then(node_pattern(expr_parser.clone()))
        .then(
            relationship_pattern(expr_parser.clone())
                .then(node_pattern(expr_parser))
                .repeated()
                .collect(),
        )
        .map(|((path_variable, node), rels)| Pattern {
            node,
            rels,
            path_variable,
        })
}

// ─── Phase 4: Clause & Helper Combinators ─────────────────────────────────────

/// Parses a RETURN projection item: `expr AS alias` or just `expr`
fn return_item<'a>() -> impl Parser<'a, ParserInput<'a>, ReturnItem, ParserError<'a>> + Clone {
    expr_parser()
        .then(keyword("AS").ignore_then(identifier()).or_not())
        .map(|(expr, alias)| ReturnItem {
            expr,
            alias,
            source_text: None,
        })
}

/// Parses the complete `RETURN` clause. `src` is the original query text, used to
/// capture each unaliased item's verbatim source as its default column name.
fn return_clause(
    src: &str,
) -> impl Parser<'_, ParserInput<'_>, ReturnClause, ParserError<'_>> + Clone {
    keyword("RETURN")
        .ignore_then(
            keyword("DISTINCT")
                .to(true)
                .or_not()
                .map(|d| d.unwrap_or(false)),
        )
        .then(choice((
            // `RETURN *`: return all bound variables.
            sym(Tok::Star).to(vec![ReturnItem {
                expr: Expr::FunctionCall {
                    name: "__star__".to_string(),
                    args: vec![],
                },
                alias: None,
                source_text: None,
            }]),
            return_item()
                .map_with(move |mut item, e| {
                    // The default column name is the verbatim source of the
                    // expression; only relevant when the item has no `AS` alias.
                    if item.alias.is_none() {
                        let sp = e.span();
                        if let Some(text) = src.get(sp.start()..sp.end()) {
                            item.source_text = Some(text.trim().to_string());
                        }
                    }
                    item
                })
                .separated_by(sym(Tok::Comma))
                .at_least(1)
                .collect::<Vec<_>>(),
        )))
        .map(|(distinct, items)| ReturnClause { items, distinct })
}

/// Parses `WHERE` filters
fn where_clause<'a>() -> impl Parser<'a, ParserInput<'a>, WhereClause, ParserError<'a>> + Clone {
    keyword("WHERE")
        .ignore_then(expr_parser())
        .map(|expr| match expr {
            Expr::BinaryOp { op, left, right } => {
                let is_comp = |o: &BinaryOperator| {
                    matches!(
                        o,
                        BinaryOperator::Eq
                            | BinaryOperator::Ne
                            | BinaryOperator::Lt
                            | BinaryOperator::Gt
                            | BinaryOperator::Le
                            | BinaryOperator::Ge
                    )
                };
                let left_is_comp = match &*left {
                    Expr::BinaryOp { op: ol, .. } => is_comp(ol),
                    _ => false,
                };
                if is_comp(&op) && left_is_comp {
                    WhereClause::Expr(Expr::BinaryOp { op, left, right })
                } else {
                    match op {
                        BinaryOperator::Eq => WhereClause::Eq(*left, *right),
                        BinaryOperator::Ne => WhereClause::Ne(*left, *right),
                        BinaryOperator::Lt => WhereClause::Lt(*left, *right),
                        BinaryOperator::Gt => WhereClause::Gt(*left, *right),
                        BinaryOperator::Le => WhereClause::Le(*left, *right),
                        BinaryOperator::Ge => WhereClause::Ge(*left, *right),
                        _ => WhereClause::Expr(Expr::BinaryOp { op, left, right }),
                    }
                }
            }
            // `n:Label` in WHERE context maps directly to an Expr so the
            // logical planner creates a HasLabel filter.
            other => WhereClause::Expr(other),
        })
}

/// Parses a `MATCH` clause
fn match_clause<'a>() -> impl Parser<'a, ParserInput<'a>, MatchClause, ParserError<'a>> + Clone {
    keyword("MATCH")
        .ignore_then(pattern(expr_parser()))
        .map(|pattern| MatchClause { pattern })
}

/// Parses the `ORDER BY` clause
fn order_by_clause<'a>() -> impl Parser<'a, ParserInput<'a>, OrderBy, ParserError<'a>> + Clone {
    keyword("ORDER")
        .ignore_then(keyword("BY"))
        .ignore_then(
            expr_parser()
                .then(
                    choice((keyword("ASC").to(true), keyword("DESC").to(false)))
                        .or_not()
                        .map(|d| d.unwrap_or(true)),
                )
                .map(|(expr, ascending)| SortItem { expr, ascending })
                .separated_by(sym(Tok::Comma))
                .at_least(1)
                .collect(),
        )
        .map(|items| OrderBy { items })
}

fn remove_item<'a>() -> impl Parser<'a, ParserInput<'a>, Vec<RemoveItem>, ParserError<'a>> + Clone {
    identifier()
        .then(choice((
            sym(Tok::Dot).ignore_then(identifier()).map(|prop| {
                vec![RemoveItem::Property {
                    variable: String::new(),
                    property: prop,
                }]
            }),
            sym(Tok::Colon)
                .ignore_then(
                    identifier()
                        .separated_by(sym(Tok::Colon))
                        .at_least(1)
                        .collect::<Vec<String>>(),
                )
                .map(|labels| {
                    labels
                        .into_iter()
                        .map(|lbl| RemoveItem::Label {
                            variable: String::new(),
                            label: lbl,
                        })
                        .collect()
                }),
        )))
        .map(|(var, mut items)| {
            for item in &mut items {
                match item {
                    RemoveItem::Property { variable, .. } => *variable = var.clone(),
                    RemoveItem::Label { variable, .. } => *variable = var.clone(),
                }
            }
            items
        })
}

fn query_part<'a>() -> impl Parser<'a, ParserInput<'a>, QueryPart, ParserError<'a>> + Clone {
    choice((
        // CALL procedure(args) [YIELD ...]. The procedure name is a dotted
        // identifier (e.g. `test.my.proc`). Arguments are optional: the
        // no-parentheses form `CALL proc` reads its arguments from query
        // parameters. YIELD selects and optionally renames output fields, or
        // `YIELD *` projects all of them.
        {
            let proc_name = identifier()
                .then(
                    sym(Tok::Dot)
                        .ignore_then(identifier())
                        .repeated()
                        .collect::<Vec<_>>(),
                )
                .map(|(first, rest)| {
                    let mut name = first;
                    for segment in rest {
                        name.push('.');
                        name.push_str(&segment);
                    }
                    name
                });

            let call_args = sym(Tok::LParen)
                .ignore_then(
                    expr_parser()
                        .separated_by(sym(Tok::Comma))
                        .allow_trailing()
                        .collect::<Vec<Expr>>(),
                )
                .then_ignore(sym(Tok::RParen));

            let yield_item = identifier().then(keyword("AS").ignore_then(identifier()).or_not());

            let yield_clause = keyword("YIELD").ignore_then(choice((
                sym(Tok::Star).to((None, true)),
                yield_item
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect::<Vec<(String, Option<String>)>>()
                    .map(|items| (Some(items), false)),
            )));

            keyword("CALL")
                .ignore_then(proc_name)
                .then(call_args.or_not())
                .then(yield_clause.or_not())
                .map(|((name, opt_args), opt_yield)| {
                    let (args, implicit_args) = match opt_args {
                        Some(a) => (a, false),
                        None => (vec![], true),
                    };
                    let (yields, yield_star) = match opt_yield {
                        Some((y, star)) => (y, star),
                        None => (None, false),
                    };
                    QueryPart::Call {
                        name,
                        args,
                        implicit_args,
                        yields,
                        yield_star,
                        resolved: None,
                    }
                })
        },
        choice((
            keyword("OPTIONAL").ignore_then(keyword("MATCH")).to(true),
            keyword("MATCH").to(false),
        ))
        .then(
            pattern(expr_parser())
                .separated_by(sym(Tok::Comma))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then(where_clause().or_not())
        .map(|((is_optional, patterns), where_clause)| {
            let match_clauses: Vec<MatchClause> = patterns
                .into_iter()
                .map(|pattern| MatchClause { pattern })
                .collect();
            if is_optional {
                QueryPart::OptionalMatch {
                    match_clauses,
                    where_clause,
                }
            } else {
                QueryPart::Match {
                    match_clauses,
                    where_clause,
                }
            }
        }),
        keyword("WITH")
            .ignore_then(
                keyword("DISTINCT")
                    .to(true)
                    .or_not()
                    .map(|d| d.unwrap_or(false)),
            )
            .then(choice((
                sym(Tok::Star).to(vec![ReturnItem {
                    expr: Expr::FunctionCall {
                        name: "__star__".to_string(),
                        args: vec![],
                    },
                    alias: None,
                    source_text: None,
                }]),
                return_item()
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )))
            .then(where_clause().or_not())
            .then(order_by_clause().or_not())
            .then(keyword("SKIP").ignore_then(expr_parser()).or_not())
            .then(keyword("LIMIT").ignore_then(expr_parser()).or_not())
            .map(
                |(((((distinct, items), where_clause), order_by), skip), limit)| QueryPart::With {
                    items,
                    where_clause,
                    order_by,
                    skip,
                    limit,
                    distinct,
                },
            ),
        keyword("UNWIND")
            .ignore_then(expr_parser())
            .then_ignore(keyword("AS"))
            .then(identifier())
            .map(|(expr, variable)| QueryPart::Unwind { expr, variable }),
        keyword("CREATE")
            .ignore_then(
                pattern(expr_parser())
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .map(|patterns| QueryPart::Create { patterns }),
        merge_statement()
            .separated_by(sym(Tok::Comma))
            .at_least(1)
            .collect::<Vec<_>>()
            .map(|merges| QueryPart::Merge { merges }),
        keyword("SET")
            .ignore_then(
                set_item()
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .map(|items| QueryPart::Set { items }),
        keyword("DETACH")
            .to(true)
            .or_not()
            .map(|d| d.unwrap_or(false))
            .then_ignore(keyword("DELETE"))
            .then(
                expr_parser()
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .map(|(detach, targets)| QueryPart::Delete { targets, detach }),
        keyword("REMOVE")
            .ignore_then(
                remove_item()
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .map(|items_list| QueryPart::Remove {
                items: items_list.into_iter().flatten().collect(),
            }),
    ))
}

/// Parses read-only `Query` statements. `src` is the original query text, threaded
/// to `return_clause` so unaliased projections capture their verbatim column names.
pub(super) fn query_parser(
    src: &str,
) -> impl Parser<'_, ParserInput<'_>, Query, ParserError<'_>> + Clone {
    // Full query: one or more query parts followed by optional RETURN.
    // If parts is non-empty and RETURN is absent this is a write-only pipeline.
    // A bare `RETURN expr` (no preceding MATCH/WITH/etc.) is handled by presenting
    // an empty parts list alongside the RETURN clause.
    let bare_return = return_clause(src)
        .then(order_by_clause().or_not())
        .then(keyword("SKIP").ignore_then(expr_parser()).or_not())
        .then(keyword("LIMIT").ignore_then(expr_parser()).or_not())
        .map(|(((return_clause, order_by), skip), limit)| Query {
            match_clauses: vec![],
            where_clause: None,
            return_clause,
            parts: vec![],
            order_by,
            skip,
            limit,
        });

    let parts_query = query_part()
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .then(return_clause(src).or_not())
        .then(order_by_clause().or_not())
        .then(keyword("SKIP").ignore_then(expr_parser()).or_not())
        .then(keyword("LIMIT").ignore_then(expr_parser()).or_not())
        .map(|((((parts, opt_return), order_by), skip), limit)| {
            let return_clause = opt_return.unwrap_or(ReturnClause {
                items: vec![],
                distinct: false,
            });
            let (match_clauses, where_clause) = parts
                .first()
                .and_then(|p| {
                    if let QueryPart::Match {
                        match_clauses,
                        where_clause,
                    } = p
                    {
                        Some((match_clauses.clone(), where_clause.clone()))
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            Query {
                match_clauses,
                where_clause,
                return_clause,
                parts,
                order_by,
                skip,
                limit,
            }
        });

    choice((parts_query, bare_return))
}

// ─── Phase 5: Write & Mutation Clauses ────────────────────────────────────────

/// Parses individual items in a `SET` update: `n.prop = expr`
fn set_item<'a>() -> impl Parser<'a, ParserInput<'a>, SetItem, ParserError<'a>> + Clone {
    // SET n.prop = expr
    let property = identifier()
        .then_ignore(sym(Tok::Dot))
        .then(identifier())
        .then_ignore(sym(Tok::Eq))
        .then(expr_parser())
        .map(|((variable, property), expr)| SetItem::Property {
            variable,
            property,
            expr,
        });

    // SET n:Label or SET n:Label1:Label2
    let labels = identifier()
        .then(
            sym(Tok::Colon)
                .ignore_then(identifier())
                .repeated()
                .at_least(1)
                .collect::<Vec<String>>(),
        )
        .map(|(variable, labels)| SetItem::Labels { variable, labels });

    choice((property, labels))
}

/// Parses the target of a schema DDL statement: either a node pattern
/// `(n:Label)` or a relationship pattern `()-[r:TYPE]-()`. Yields the label
/// or type name together with which kind of element it names.
fn schema_target<'a>()
-> impl Parser<'a, ParserInput<'a>, (String, SchemaTarget), ParserError<'a>> + Clone {
    let node = sym(Tok::LParen)
        .ignore_then(identifier()) // var
        .ignore_then(sym(Tok::Colon))
        .ignore_then(identifier()) // label
        .then_ignore(sym(Tok::RParen))
        .map(|label| (label, SchemaTarget::Node));

    let relationship = sym(Tok::LParen)
        .ignore_then(sym(Tok::RParen))
        .ignore_then(sym(Tok::Minus))
        .ignore_then(sym(Tok::LBrack))
        .ignore_then(identifier()) // var
        .ignore_then(sym(Tok::Colon))
        .ignore_then(identifier()) // type
        .then_ignore(sym(Tok::RBrack))
        .then_ignore(sym(Tok::Minus))
        .then_ignore(sym(Tok::LParen))
        .then_ignore(sym(Tok::RParen))
        .map(|etype| (etype, SchemaTarget::Relationship));

    choice((node, relationship))
}

/// Parses a `CREATE INDEX` or `CREATE CONSTRAINT` or standard `CREATE` statement
fn create_statement<'a>() -> impl Parser<'a, ParserInput<'a>, Statement, ParserError<'a>> + Clone {
    let index_ddl = keyword("CREATE")
        .ignore_then(keyword("INDEX"))
        .ignore_then(keyword("FOR"))
        .ignore_then(schema_target())
        .then_ignore(keyword("ON"))
        .then_ignore(sym(Tok::LParen))
        .then_ignore(identifier()) // var
        .then_ignore(sym(Tok::Dot))
        .then(identifier()) // property
        .then_ignore(sym(Tok::RParen))
        .map(|((label, target), property)| {
            Statement::CreateIndex(CreateIndexStatement {
                label,
                property,
                target,
            })
        });

    let constraint_ddl = keyword("CREATE")
        .ignore_then(keyword("CONSTRAINT"))
        .ignore_then(keyword("ON"))
        .ignore_then(schema_target())
        .then_ignore(keyword("ASSERT"))
        .then(choice((
            // EXISTS(n.prop)
            keyword("EXISTS")
                .ignore_then(sym(Tok::LParen))
                .ignore_then(identifier())
                .ignore_then(sym(Tok::Dot))
                .ignore_then(identifier())
                .then_ignore(sym(Tok::RParen))
                .map(|prop| (prop, ConstraintKind::Exists)),
            // n.prop IS UNIQUE
            identifier()
                .ignore_then(sym(Tok::Dot))
                .ignore_then(identifier())
                .then_ignore(keyword("IS"))
                .then_ignore(keyword("UNIQUE"))
                .map(|prop| (prop, ConstraintKind::Unique)),
        )))
        .map(|((label, target), (property, kind))| {
            Statement::CreateConstraint(CreateConstraintStatement {
                label,
                property,
                kind,
                target,
            })
        });

    let normal_create = keyword("CREATE")
        .ignore_then(
            pattern(expr_parser())
                .separated_by(sym(Tok::Comma))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .map(|patterns| Statement::Create(CreateStatement { patterns }));

    choice((index_ddl, constraint_ddl, normal_create))
}

/// Parses a `DROP INDEX` or `DROP CONSTRAINT` statement
fn drop_statement<'a>() -> impl Parser<'a, ParserInput<'a>, Statement, ParserError<'a>> + Clone {
    let index_ddl = keyword("DROP")
        .ignore_then(keyword("INDEX"))
        .ignore_then(keyword("FOR"))
        .ignore_then(schema_target())
        .then_ignore(keyword("ON"))
        .then_ignore(sym(Tok::LParen))
        .then_ignore(identifier()) // var
        .then_ignore(sym(Tok::Dot))
        .then(identifier()) // property
        .then_ignore(sym(Tok::RParen))
        .map(|((label, target), property)| {
            Statement::DropIndex(DropIndexStatement {
                label,
                property,
                target,
            })
        });

    let constraint_ddl = keyword("DROP")
        .ignore_then(keyword("CONSTRAINT"))
        .ignore_then(keyword("ON"))
        .ignore_then(schema_target())
        .then_ignore(keyword("ASSERT"))
        .then(choice((
            // EXISTS(n.prop)
            keyword("EXISTS")
                .ignore_then(sym(Tok::LParen))
                .ignore_then(identifier())
                .ignore_then(sym(Tok::Dot))
                .ignore_then(identifier())
                .then_ignore(sym(Tok::RParen))
                .map(|prop| (prop, ConstraintKind::Exists)),
            // n.prop IS UNIQUE
            identifier()
                .ignore_then(sym(Tok::Dot))
                .ignore_then(identifier())
                .then_ignore(keyword("IS"))
                .then_ignore(keyword("UNIQUE"))
                .map(|prop| (prop, ConstraintKind::Unique)),
        )))
        .map(|((label, target), (property, kind))| {
            Statement::DropConstraint(DropConstraintStatement {
                label,
                property,
                kind,
                target,
            })
        });

    choice((index_ddl, constraint_ddl))
}

/// Parses a `COPY <LabelName> FROM '<filepath>' [WITH <options_map>]` statement
fn copy_statement<'a>() -> impl Parser<'a, ParserInput<'a>, Statement, ParserError<'a>> + Clone {
    keyword("COPY")
        .ignore_then(identifier())
        .then_ignore(keyword("FROM"))
        .then(any().filter_map(|tok| match tok {
            Tok::Str(s) => Some(s.clone()),
            _ => None,
        }))
        .then(
            keyword("WITH")
                .ignore_then(property_map(expr_parser()))
                .or_not(),
        )
        .map(|((target, filepath), options)| {
            Statement::Copy(CopyStatement {
                target,
                filepath,
                options,
            })
        })
}

/// Parses a `EXPORT DATABASE '<filepath>' [WITH <options_map>]` statement
fn export_database_statement<'a>()
-> impl Parser<'a, ParserInput<'a>, Statement, ParserError<'a>> + Clone {
    keyword("EXPORT")
        .ignore_then(keyword("DATABASE"))
        .ignore_then(any().filter_map(|tok| match tok {
            Tok::Str(s) => Some(s.clone()),
            _ => None,
        }))
        .then(
            keyword("WITH")
                .ignore_then(property_map(expr_parser()))
                .or_not(),
        )
        .map(|(filepath, options)| {
            Statement::ExportDatabase(ExportDatabaseStatement { filepath, options })
        })
}

/// Parses a `IMPORT DATABASE '<filepath>'` statement
fn import_database_statement<'a>()
-> impl Parser<'a, ParserInput<'a>, Statement, ParserError<'a>> + Clone {
    keyword("IMPORT")
        .ignore_then(keyword("DATABASE"))
        .ignore_then(any().filter_map(|tok| match tok {
            Tok::Str(s) => Some(s.clone()),
            _ => None,
        }))
        .map(|filepath| Statement::ImportDatabase(ImportDatabaseStatement { filepath }))
}

/// Parses a `DELETE` clause / statement
fn delete_statement<'a>() -> impl Parser<'a, ParserInput<'a>, Statement, ParserError<'a>> + Clone {
    let detach = keyword("DETACH")
        .to(true)
        .or_not()
        .map(|d| d.unwrap_or(false));

    keyword("MATCH")
        .ignore_then(
            pattern(expr_parser())
                .separated_by(sym(Tok::Comma))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then(where_clause().or_not())
        .then(detach)
        .then_ignore(keyword("DELETE"))
        .then(
            expr_parser()
                .separated_by(sym(Tok::Comma))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .map(|(((match_clauses, where_clause), detach), targets)| {
            let mut clauses = Vec::new();
            for pat in match_clauses {
                clauses.push(MatchClause { pattern: pat });
            }
            Statement::Delete(DeleteStatement {
                match_clauses: clauses,
                where_clause,
                targets,
                detach,
            })
        })
}

/// Parses a `MERGE` clause / statement
fn merge_statement<'a>() -> impl Parser<'a, ParserInput<'a>, MergeStatement, ParserError<'a>> + Clone
{
    // An `ON CREATE SET ...` or `ON MATCH SET ...` action. Both may appear, in
    // either order, so they are parsed as a repeated sequence and folded.
    let set_items = || {
        set_item()
            .separated_by(sym(Tok::Comma))
            .at_least(1)
            .collect::<Vec<_>>()
    };
    let on_action = keyword("ON").ignore_then(choice((
        keyword("CREATE")
            .ignore_then(keyword("SET"))
            .ignore_then(set_items())
            .map(|items| (true, items)),
        keyword("MATCH")
            .ignore_then(keyword("SET"))
            .ignore_then(set_items())
            .map(|items| (false, items)),
    )));

    keyword("MERGE")
        .ignore_then(pattern(expr_parser()))
        .then(on_action.repeated().collect::<Vec<(bool, Vec<_>)>>())
        .map(|(pattern, actions)| {
            let mut on_create_set = Vec::new();
            let mut on_match_set = Vec::new();
            for (is_create, items) in actions {
                if is_create {
                    on_create_set.extend(items);
                } else {
                    on_match_set.extend(items);
                }
            }
            MergeStatement {
                pattern,
                on_create_set,
                on_match_set,
            }
        })
}

// ─── Phase 6: UNION Chaining & Pipelines ───────────────────────────────────────

/// Zero-width parser that succeeds only at a statement boundary: end of input, a
/// `;` pipeline separator, or a `UNION` keyword. It consumes nothing (the `;` and
/// `UNION` checks rewind), so the caller's `foldl` / `separated_by` combinators
/// still see the boundary token.
///
/// The specialized statement parsers (`CREATE ... RETURN`, `MATCH ... SET`, single
/// `MERGE`, etc.) match a fixed clause shape. Without this guard they commit to a
/// prefix of a longer multi-clause query such as `CREATE (a) CREATE (b)` or
/// `MATCH (a) SET a.x = 1 WITH a RETURN a`, leaving trailing clauses that the
/// top-level parser then rejects with "expected end of input". Requiring a boundary
/// makes those parsers fail when more clauses follow, so the general `query_parser`
/// clause-sequence path handles the full query instead.
fn at_statement_boundary<'a>() -> impl Parser<'a, ParserInput<'a>, (), ParserError<'a>> + Clone {
    choice((end(), sym(Tok::Semi).rewind(), keyword("UNION").rewind()))
}

/// Parses a top-level single statement (excluding pipeline chaining), supporting recursive `UNION` / `UNION ALL`.
/// `src` is the original query text, threaded to `return_clause` for verbatim column naming.
fn statement_union_parser(
    src: &str,
) -> impl Parser<'_, ParserInput<'_>, Statement, ParserError<'_>> + Clone {
    recursive(|statement| {
        let create_return = keyword("CREATE")
            .ignore_then(
                pattern(expr_parser())
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect(),
            )
            .then(return_clause(src))
            .then(order_by_clause().or_not())
            .then(keyword("SKIP").ignore_then(expr_parser()).or_not())
            .then(keyword("LIMIT").ignore_then(expr_parser()).or_not())
            .map(|((((patterns, return_clause), order_by), skip), limit)| {
                Statement::CreateAndReturn(CreateAndReturnStatement {
                    patterns,
                    return_clause,
                    order_by,
                    skip,
                    limit,
                })
            });

        let match_set_return = match_clause()
            .repeated()
            .at_least(1)
            .collect()
            .then(where_clause().or_not())
            .then_ignore(keyword("SET"))
            .then(
                set_item()
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect(),
            )
            .then(return_clause(src))
            .then(order_by_clause().or_not())
            .then(keyword("SKIP").ignore_then(expr_parser()).or_not())
            .then(keyword("LIMIT").ignore_then(expr_parser()).or_not())
            .map(
                |(
                    (((((match_clauses, where_clause), set_items), return_clause), order_by), skip),
                    limit,
                )| {
                    Statement::SetAndReturn(SetAndReturnStatement {
                        match_clauses,
                        where_clause,
                        set_items,
                        return_clause,
                        order_by,
                        skip,
                        limit,
                    })
                },
            );

        let match_delete_return = match_clause()
            .repeated()
            .at_least(1)
            .collect()
            .then(where_clause().or_not())
            .then(
                keyword("DETACH")
                    .to(true)
                    .or_not()
                    .map(|d| d.unwrap_or(false)),
            )
            .then_ignore(keyword("DELETE"))
            .then(
                expr_parser()
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect(),
            )
            .then(return_clause(src))
            .then(order_by_clause().or_not())
            .then(keyword("SKIP").ignore_then(expr_parser()).or_not())
            .then(keyword("LIMIT").ignore_then(expr_parser()).or_not())
            .map(
                |(
                    (
                        (
                            ((((match_clauses, where_clause), detach), targets), return_clause),
                            order_by,
                        ),
                        skip,
                    ),
                    limit,
                )| {
                    Statement::DeleteAndReturn(DeleteAndReturnStatement {
                        match_clauses,
                        where_clause,
                        targets,
                        detach,
                        return_clause,
                        order_by,
                        skip,
                        limit,
                    })
                },
            );

        let match_remove_return = match_clause()
            .repeated()
            .at_least(1)
            .collect()
            .then(where_clause().or_not())
            .then_ignore(keyword("REMOVE"))
            .then(
                remove_item()
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .then(return_clause(src))
            .then(order_by_clause().or_not())
            .then(keyword("SKIP").ignore_then(expr_parser()).or_not())
            .then(keyword("LIMIT").ignore_then(expr_parser()).or_not())
            .map(
                |(
                    (
                        (
                            (((match_clauses, where_clause), remove_items_list), return_clause),
                            order_by,
                        ),
                        skip,
                    ),
                    limit,
                )| {
                    Statement::RemoveAndReturn(RemoveAndReturnStatement {
                        match_clauses,
                        where_clause,
                        items: remove_items_list.into_iter().flatten().collect(),
                        return_clause,
                        order_by,
                        skip,
                        limit,
                    })
                },
            );

        let merge_return = merge_statement()
            .then(return_clause(src))
            .then(order_by_clause().or_not())
            .then(keyword("SKIP").ignore_then(expr_parser()).or_not())
            .then(keyword("LIMIT").ignore_then(expr_parser()).or_not())
            .map(|((((merge, return_clause), order_by), skip), limit)| {
                Statement::MergeAndReturn(MergeAndReturnStatement {
                    merges: vec![merge],
                    return_clause,
                    order_by,
                    skip,
                    limit,
                })
            });

        let foreach_stmt = keyword("FOREACH")
            .ignore_then(sym(Tok::LParen))
            .ignore_then(identifier())
            .then_ignore(keyword("IN"))
            .then(expr_parser())
            .then_ignore(sym(Tok::Pipe))
            .then(statement.clone())
            .then_ignore(sym(Tok::RParen))
            .map(|((variable, list), body_stmt)| {
                Statement::Foreach(ForeachStatement {
                    variable,
                    list,
                    body: vec![body_stmt],
                })
            });

        let set_stmt = match_clause()
            .repeated()
            .at_least(1)
            .collect()
            .then(where_clause().or_not())
            .then_ignore(keyword("SET"))
            .then(
                set_item()
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .map(|((match_clauses, where_clause), set_items)| {
                Statement::Set(SetStatement {
                    match_clauses,
                    where_clause,
                    set_items,
                })
            });

        let remove_stmt = match_clause()
            .repeated()
            .at_least(1)
            .collect()
            .then(where_clause().or_not())
            .then_ignore(keyword("REMOVE"))
            .then(
                remove_item()
                    .separated_by(sym(Tok::Comma))
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .map(|((match_clauses, where_clause), remove_items_list)| {
                Statement::Remove(RemoveStatement {
                    match_clauses,
                    where_clause,
                    items: remove_items_list.into_iter().flatten().collect(),
                })
            });

        let write_stmt = choice((
            create_statement(),
            delete_statement(),
            merge_statement().map(Statement::Merge),
            set_stmt,
            remove_stmt,
            foreach_stmt,
            drop_statement(),
            copy_statement(),
            export_database_statement(),
            import_database_statement(),
        ));

        // Each specialized parser is guarded by `at_statement_boundary` so it only wins
        // when it consumes a complete statement. If more clauses follow (e.g.
        // `CREATE (a) CREATE (b)` or `MATCH (a) SET a.x = 1 WITH a RETURN a`), the guard
        // fails and the general `query_parser` clause-sequence path handles the full
        // query. The dedicated variants still fire (with their proper write-lock
        // semantics) for the single-statement shapes they were written for.
        let base_stmt = choice((
            create_return.then_ignore(at_statement_boundary()),
            match_set_return.then_ignore(at_statement_boundary()),
            match_delete_return.then_ignore(at_statement_boundary()),
            match_remove_return.then_ignore(at_statement_boundary()),
            merge_return.then_ignore(at_statement_boundary()),
            write_stmt.then_ignore(at_statement_boundary()),
            query_parser(src).map(Statement::Query),
        ));

        base_stmt.foldl(
            keyword("UNION")
                .ignore_then(keyword("ALL").to(true).or_not().map(|a| a.unwrap_or(false)))
                .then(statement.clone())
                .repeated(),
            |left, (all, right)| {
                Statement::Union(UnionStatement {
                    left: Box::new(left),
                    right: Box::new(right),
                    all,
                })
            },
        )
    })
}

/// Parses one or more semicolon-separated statements (Cypher execution pipelines).
/// `src` is the original query text, threaded for verbatim column naming.
pub(crate) fn pipeline_parser(
    src: &str,
) -> impl Parser<'_, ParserInput<'_>, Statement, ParserError<'_>> + Clone {
    statement_union_parser(src)
        .separated_by(sym(Tok::Semi).repeated().at_least(1))
        .allow_trailing()
        .collect::<Vec<Statement>>()
        .map(|mut stmts| {
            if stmts.len() == 1 {
                stmts.remove(0)
            } else {
                Statement::Pipeline(stmts)
            }
        })
}

// ─── Variable validation ──────────────────────────────────────────────────────

pub(crate) fn validate_match_clause_variables(clauses: &[MatchClause]) -> Result<(), String> {
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

pub(crate) fn validate_cross_clause_variable_types(parts: &[QueryPart]) -> Result<(), String> {
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
            QueryPart::Create { .. }
            | QueryPart::Merge { .. }
            | QueryPart::Set { .. }
            | QueryPart::Delete { .. }
            | QueryPart::Remove { .. }
            | QueryPart::Call { .. } => None,
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

/// Collect the free variable names referenced in an expression. A bare variable is
/// `Expr::Prop(name, "")` and a property access is `Expr::Prop(name, "prop")`, so both
/// contribute `name`. Variables bound locally by list comprehensions and quantifiers are
/// not free and are excluded.
fn collect_expr_vars(expr: &Expr, out: &mut std::collections::HashSet<String>) {
    match expr {
        Expr::Prop(var, _) => {
            out.insert(var.clone());
        }
        Expr::HasLabel { variable, .. } => {
            out.insert(variable.clone());
        }
        Expr::Agg(_, inner) | Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            collect_expr_vars(inner, out)
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_expr_vars(left, out);
            collect_expr_vars(right, out);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_expr_vars(a, out);
            }
        }
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            if let Some(s) = subject {
                collect_expr_vars(s, out);
            }
            for a in arms {
                collect_expr_vars(&a.when, out);
                collect_expr_vars(&a.then, out);
            }
            if let Some(e) = else_expr {
                collect_expr_vars(e, out);
            }
        }
        Expr::Subscript { expr, index } => {
            collect_expr_vars(expr, out);
            collect_expr_vars(index, out);
        }
        Expr::Slice { expr, start, end } => {
            collect_expr_vars(expr, out);
            if let Some(s) = start {
                collect_expr_vars(s, out);
            }
            if let Some(e) = end {
                collect_expr_vars(e, out);
            }
        }
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            transform,
        } => {
            collect_expr_vars(list, out);
            let mut inner = std::collections::HashSet::new();
            if let Some(p) = predicate {
                collect_expr_vars(p, &mut inner);
            }
            if let Some(t) = transform {
                collect_expr_vars(t, &mut inner);
            }
            inner.remove(variable);
            out.extend(inner);
        }
        Expr::Reduce {
            accumulator,
            initial,
            variable,
            list,
            expression,
        } => {
            collect_expr_vars(initial, out);
            collect_expr_vars(list, out);
            let mut inner = std::collections::HashSet::new();
            collect_expr_vars(expression, &mut inner);
            inner.remove(accumulator);
            inner.remove(variable);
            out.extend(inner);
        }
        Expr::Quantifier {
            variable,
            list,
            predicate,
            ..
        } => {
            collect_expr_vars(list, out);
            let mut inner = std::collections::HashSet::new();
            collect_expr_vars(predicate, &mut inner);
            inner.remove(variable);
            out.extend(inner);
        }
        // The anchor node references an outer variable and is collected as free. The
        // relationship, target-node, and path variables are bound locally, so they are
        // removed from whatever the predicate, transform, and inline property expressions
        // reference.
        Expr::PatternComprehension {
            pattern,
            predicate,
            transform,
        } => {
            if let Some(anchor) = &pattern.node.variable {
                out.insert(anchor.clone());
            }
            let mut local = std::collections::HashSet::new();
            if let Some(pv) = &pattern.path_variable {
                local.insert(pv.clone());
            }
            for (rel, node) in &pattern.rels {
                if let Some(v) = &rel.variable {
                    local.insert(v.clone());
                }
                if let Some(v) = &node.variable {
                    local.insert(v.clone());
                }
            }
            let mut inner = std::collections::HashSet::new();
            if let Some(p) = predicate {
                collect_expr_vars(p, &mut inner);
            }
            collect_expr_vars(transform, &mut inner);
            if let Some(props) = &pattern.node.properties {
                for e in props.values() {
                    collect_expr_vars(e, &mut inner);
                }
            }
            for (rel, node) in &pattern.rels {
                if let Some(props) = &rel.properties {
                    for e in props.values() {
                        collect_expr_vars(e, &mut inner);
                    }
                }
                if let Some(props) = &node.properties {
                    for e in props.values() {
                        collect_expr_vars(e, &mut inner);
                    }
                }
            }
            for v in &local {
                inner.remove(v);
            }
            out.extend(inner);
        }
        Expr::Literal(_) | Expr::Param(_) | Expr::CountStar => {}
    }
}

/// Collect the node, relationship, and path variable names bound by a pattern.
fn collect_pattern_vars(pattern: &Pattern, out: &mut std::collections::HashSet<String>) {
    if let Some(v) = &pattern.node.variable {
        out.insert(v.clone());
    }
    if let Some(pv) = &pattern.path_variable {
        out.insert(pv.clone());
    }
    for (rel, target) in &pattern.rels {
        if let Some(v) = &rel.variable {
            out.insert(v.clone());
        }
        if let Some(v) = &target.variable {
            out.insert(v.clone());
        }
    }
}

/// The set of variable names a WITH projection brings into its output scope. Returns `None`
/// when the projection includes the `*` wildcard (the `__star__` sentinel), meaning every
/// upstream variable stays in scope and the ORDER BY scope cannot be restricted.
fn with_output_scope(items: &[ReturnItem]) -> Option<std::collections::HashSet<String>> {
    let mut scope = std::collections::HashSet::new();
    for item in items {
        if let Expr::FunctionCall { name, .. } = &item.expr {
            if name == "__star__" {
                return None;
            }
        }
        if let Some(alias) = &item.alias {
            scope.insert(alias.clone());
        } else if let Expr::Prop(var, prop) = &item.expr {
            if prop.is_empty() {
                scope.insert(var.clone());
            }
        }
    }
    Some(scope)
}

/// Validate that every variable referenced in a WITH-clause ORDER BY is in scope: either
/// projected by that WITH (output scope) or bound in the pipeline before it (input scope).
/// A reference to an out-of-scope variable (bound upstream but dropped by an intervening
/// projection) or a never-defined variable raises a compile-time `UndefinedVariable` error.
fn validate_order_by_scope(parts: &[QueryPart]) -> Result<(), String> {
    let mut bound: std::collections::HashSet<String> = std::collections::HashSet::new();
    for part in parts {
        match part {
            QueryPart::Match { match_clauses, .. }
            | QueryPart::OptionalMatch { match_clauses, .. } => {
                for mc in match_clauses {
                    collect_pattern_vars(&mc.pattern, &mut bound);
                }
            }
            QueryPart::Unwind { variable, .. } => {
                bound.insert(variable.clone());
            }
            QueryPart::Create { patterns } => {
                for p in patterns {
                    collect_pattern_vars(p, &mut bound);
                }
            }
            QueryPart::Merge { merges } => {
                for m in merges {
                    collect_pattern_vars(&m.pattern, &mut bound);
                }
            }
            QueryPart::With {
                items, order_by, ..
            } => {
                let output = with_output_scope(items);
                if let (Some(ob), Some(out)) = (order_by, &output) {
                    let mut refs = std::collections::HashSet::new();
                    for si in &ob.items {
                        collect_expr_vars(&si.expr, &mut refs);
                    }
                    if let Some(missing) = refs
                        .iter()
                        .find(|v| !bound.contains(*v) && !out.contains(*v))
                    {
                        return Err(format!(
                            "SyntaxError(UndefinedVariable): variable '{}' referenced in \
                             ORDER BY is not in scope",
                            missing
                        ));
                    }
                }

                let with_has_agg = items.iter().any(|item| expr_has_aggregation(&item.expr));
                let order_by_has_agg = order_by
                    .as_ref()
                    .is_some_and(|ob| ob.items.iter().any(|si| expr_has_aggregation(&si.expr)));
                if order_by_has_agg && !with_has_agg {
                    return Err("SyntaxError: aggregation in ORDER BY is not allowed when WITH has no aggregation".to_string());
                }

                let has_with_agg = with_has_agg || order_by_has_agg;
                if has_with_agg {
                    if let Some(ob) = order_by {
                        let grouping_keys = get_grouping_keys(items);
                        let aliases: std::collections::HashSet<String> =
                            items.iter().filter_map(|item| item.alias.clone()).collect();
                        for si in &ob.items {
                            let mut non_agg_props = Vec::new();
                            collect_non_agg_props_in_expr(&si.expr, &mut non_agg_props);
                            for (var, prop) in non_agg_props {
                                let is_valid = if prop.is_empty() {
                                    grouping_keys.contains(&var) || aliases.contains(&var)
                                } else {
                                    grouping_keys.contains(&format!("{}.{}", var, prop))
                                        || grouping_keys.contains(&var)
                                        || aliases.contains(&var)
                                };
                                if !is_valid {
                                    return Err(format!(
                                        "SyntaxError: variable '{}' referenced in aggregating ORDER BY \
                                         is not a grouping key in WITH",
                                        var
                                    ));
                                }
                            }
                        }
                    }
                }

                // Apply the WITH scope barrier: the output scope replaces the input scope,
                // except for `WITH *`, which keeps upstream variables and adds any aliases.
                match output {
                    Some(out) => bound = out,
                    None => {
                        for item in items {
                            if let Some(a) = &item.alias {
                                bound.insert(a.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn expr_has_aggregation(expr: &Expr) -> bool {
    match expr {
        Expr::CountStar | Expr::Agg(_, _) => true,
        Expr::BinaryOp { left, right, .. } => {
            expr_has_aggregation(left) || expr_has_aggregation(right)
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            expr_has_aggregation(inner)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_aggregation),
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            subject.as_ref().is_some_and(|s| expr_has_aggregation(s))
                || arms
                    .iter()
                    .any(|a| expr_has_aggregation(&a.when) || expr_has_aggregation(&a.then))
                || else_expr.as_ref().is_some_and(|e| expr_has_aggregation(e))
        }
        Expr::Subscript { expr, index } => {
            expr_has_aggregation(expr) || expr_has_aggregation(index)
        }
        Expr::Slice { expr, start, end } => {
            expr_has_aggregation(expr)
                || start.as_ref().is_some_and(|s| expr_has_aggregation(s))
                || end.as_ref().is_some_and(|e| expr_has_aggregation(e))
        }
        Expr::ListComprehension {
            list,
            predicate,
            transform,
            ..
        } => {
            expr_has_aggregation(list)
                || predicate.as_ref().is_some_and(|p| expr_has_aggregation(p))
                || transform.as_ref().is_some_and(|t| expr_has_aggregation(t))
        }
        Expr::Reduce {
            initial,
            list,
            expression,
            ..
        } => {
            expr_has_aggregation(initial)
                || expr_has_aggregation(list)
                || expr_has_aggregation(expression)
        }
        Expr::Quantifier {
            list, predicate, ..
        } => expr_has_aggregation(list) || expr_has_aggregation(predicate),
        _ => false,
    }
}

fn collect_non_agg_props_in_expr(expr: &Expr, props: &mut Vec<(String, String)>) {
    match expr {
        Expr::CountStar | Expr::Agg(_, _) => {}
        Expr::Prop(var, prop) => {
            props.push((var.clone(), prop.clone()));
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_non_agg_props_in_expr(left, props);
            collect_non_agg_props_in_expr(right, props);
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            collect_non_agg_props_in_expr(inner, props);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_non_agg_props_in_expr(arg, props);
            }
        }
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            if let Some(s) = subject {
                collect_non_agg_props_in_expr(s, props);
            }
            for arm in arms {
                collect_non_agg_props_in_expr(&arm.when, props);
                collect_non_agg_props_in_expr(&arm.then, props);
            }
            if let Some(e) = else_expr {
                collect_non_agg_props_in_expr(e, props);
            }
        }
        Expr::Subscript { expr, index } => {
            collect_non_agg_props_in_expr(expr, props);
            collect_non_agg_props_in_expr(index, props);
        }
        Expr::Slice { expr, start, end } => {
            collect_non_agg_props_in_expr(expr, props);
            if let Some(s) = start {
                collect_non_agg_props_in_expr(s, props);
            }
            if let Some(e) = end {
                collect_non_agg_props_in_expr(e, props);
            }
        }
        Expr::ListComprehension {
            list,
            predicate,
            transform,
            ..
        } => {
            collect_non_agg_props_in_expr(list, props);
            if let Some(p) = predicate {
                collect_non_agg_props_in_expr(p, props);
            }
            if let Some(t) = transform {
                collect_non_agg_props_in_expr(t, props);
            }
        }
        Expr::Reduce {
            initial,
            list,
            expression,
            ..
        } => {
            collect_non_agg_props_in_expr(initial, props);
            collect_non_agg_props_in_expr(list, props);
            collect_non_agg_props_in_expr(expression, props);
        }
        Expr::Quantifier {
            list, predicate, ..
        } => {
            collect_non_agg_props_in_expr(list, props);
            collect_non_agg_props_in_expr(predicate, props);
        }
        // A pattern comprehension depends only on its anchor (an outer variable) and
        // on whatever its predicate, transform, and inline properties reference from
        // the outer scope; its own relationship, target-node, and path variables are
        // local bindings and never group keys.
        Expr::PatternComprehension {
            pattern,
            predicate,
            transform,
        } => {
            let mut local = std::collections::HashSet::new();
            if let Some(pv) = &pattern.path_variable {
                local.insert(pv.clone());
            }
            for (rel, node) in &pattern.rels {
                if let Some(v) = &rel.variable {
                    local.insert(v.clone());
                }
                if let Some(v) = &node.variable {
                    local.insert(v.clone());
                }
            }
            let mut inner = Vec::new();
            if let Some(p) = predicate {
                collect_non_agg_props_in_expr(p, &mut inner);
            }
            collect_non_agg_props_in_expr(transform, &mut inner);
            if let Some(ps) = &pattern.node.properties {
                for e in ps.values() {
                    collect_non_agg_props_in_expr(e, &mut inner);
                }
            }
            for (rel, node) in &pattern.rels {
                if let Some(ps) = &rel.properties {
                    for e in ps.values() {
                        collect_non_agg_props_in_expr(e, &mut inner);
                    }
                }
                if let Some(ps) = &node.properties {
                    for e in ps.values() {
                        collect_non_agg_props_in_expr(e, &mut inner);
                    }
                }
            }
            inner.retain(|(v, _)| !local.contains(v));
            props.extend(inner);
            if let Some(anchor) = &pattern.node.variable {
                props.push((anchor.clone(), String::new()));
            }
        }
        Expr::HasLabel { variable, .. } => {
            props.push((variable.clone(), "".to_string()));
        }
        Expr::Literal(_) | Expr::Param(_) => {}
    }
}

fn get_grouping_keys(items: &[crate::ast::ReturnItem]) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    for item in items {
        if !expr_has_aggregation(&item.expr) {
            if let Some(alias) = &item.alias {
                keys.insert(alias.clone());
            }
            if let Expr::Prop(var, prop) = &item.expr {
                if prop.is_empty() {
                    keys.insert(var.clone());
                } else {
                    keys.insert(format!("{}.{}", var, prop));
                }
            }
        }
    }
    keys
}

fn validate_query_order_by(query: &Query) -> Result<(), String> {
    let mut bound = std::collections::HashSet::new();
    for part in &query.parts {
        match part {
            QueryPart::Match { match_clauses, .. }
            | QueryPart::OptionalMatch { match_clauses, .. } => {
                for mc in match_clauses {
                    collect_pattern_vars(&mc.pattern, &mut bound);
                }
            }
            QueryPart::Unwind { variable, .. } => {
                bound.insert(variable.clone());
            }
            QueryPart::Create { patterns } => {
                for p in patterns {
                    collect_pattern_vars(p, &mut bound);
                }
            }
            QueryPart::Merge { merges } => {
                for m in merges {
                    collect_pattern_vars(&m.pattern, &mut bound);
                }
            }
            QueryPart::With { items, .. } => {
                let output = with_output_scope(items);
                match output {
                    Some(out) => bound = out,
                    None => {
                        for item in items {
                            if let Some(a) = &item.alias {
                                bound.insert(a.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let is_return_star = query.return_clause.items.len() == 1
        && matches!(
            &query.return_clause.items[0].expr,
            Expr::FunctionCall { name, .. } if name == "__star__"
        );
    if is_return_star && bound.is_empty() {
        return Err(
            "SyntaxError(NoVariablesInScope): RETURN * without variables in scope is not allowed"
                .to_string(),
        );
    }

    if let Some(ref ob) = query.order_by {
        let mut out = std::collections::HashSet::new();
        for item in &query.return_clause.items {
            if let Some(a) = &item.alias {
                out.insert(a.clone());
            } else if let Expr::Prop(var, prop) = &item.expr {
                if prop.is_empty() {
                    out.insert(var.clone());
                } else {
                    out.insert(format!("{}.{}", var, prop));
                }
            }
        }

        let mut refs = std::collections::HashSet::new();
        for si in &ob.items {
            collect_expr_vars(&si.expr, &mut refs);
        }
        if let Some(missing) = refs
            .iter()
            .find(|v| !bound.contains(*v) && !out.contains(*v))
        {
            return Err(format!(
                "SyntaxError(UndefinedVariable): variable '{}' referenced in ORDER BY is not in scope",
                missing
            ));
        }

        if query.return_clause.distinct {
            for si in &ob.items {
                let mut props = Vec::new();
                collect_non_agg_props_in_expr(&si.expr, &mut props);
                for (var, prop) in props {
                    let is_valid = if prop.is_empty() {
                        out.contains(&var)
                    } else {
                        out.contains(&format!("{}.{}", var, prop)) || out.contains(&var)
                    };
                    if !is_valid {
                        return Err(format!(
                            "SyntaxError(UndefinedVariable): variable '{}' referenced in ORDER BY is not in DISTINCT scope",
                            var
                        ));
                    }
                }
            }
        }

        let return_has_agg = query
            .return_clause
            .items
            .iter()
            .any(|item| expr_has_aggregation(&item.expr));
        let order_by_has_agg = ob.items.iter().any(|si| expr_has_aggregation(&si.expr));
        if order_by_has_agg && !return_has_agg {
            return Err("SyntaxError: aggregation in ORDER BY is not allowed when RETURN has no aggregation".to_string());
        }

        let has_agg = return_has_agg || order_by_has_agg;
        if has_agg {
            let grouping_keys = get_grouping_keys(&query.return_clause.items);
            let aliases: std::collections::HashSet<String> = query
                .return_clause
                .items
                .iter()
                .filter_map(|item| item.alias.clone())
                .collect();
            for si in &ob.items {
                let mut non_agg_props = Vec::new();
                collect_non_agg_props_in_expr(&si.expr, &mut non_agg_props);
                for (var, prop) in non_agg_props {
                    let is_valid = if prop.is_empty() {
                        grouping_keys.contains(&var) || aliases.contains(&var)
                    } else {
                        grouping_keys.contains(&format!("{}.{}", var, prop))
                            || grouping_keys.contains(&var)
                            || aliases.contains(&var)
                    };
                    if !is_valid {
                        return Err(format!(
                            "SyntaxError: variable '{}' referenced in aggregating ORDER BY \
                             is not a grouping key in RETURN",
                            var
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn collect_node_and_rel_vars_in_stmt(
    stmt: &Statement,
    node_vars: &mut std::collections::HashSet<String>,
    rel_vars: &mut std::collections::HashSet<String>,
) {
    let mut collect_pattern = |pat: &Pattern| {
        if let Some(ref v) = pat.node.variable {
            node_vars.insert(v.clone());
        }
        for (rel, target) in &pat.rels {
            if let Some(ref v) = rel.variable {
                rel_vars.insert(v.clone());
            }
            if let Some(ref v) = target.variable {
                node_vars.insert(v.clone());
            }
        }
    };

    match stmt {
        Statement::Query(q) => {
            for mc in &q.match_clauses {
                collect_pattern(&mc.pattern);
            }
            for part in &q.parts {
                match part {
                    QueryPart::Match { match_clauses, .. }
                    | QueryPart::OptionalMatch { match_clauses, .. } => {
                        for mc in match_clauses {
                            collect_pattern(&mc.pattern);
                        }
                    }
                    QueryPart::Create { patterns } => {
                        for pat in patterns {
                            collect_pattern(pat);
                        }
                    }
                    QueryPart::Merge { merges } => {
                        for m in merges {
                            collect_pattern(&m.pattern);
                        }
                    }
                    _ => {}
                }
            }
        }
        Statement::Create(c) => {
            for pat in &c.patterns {
                collect_pattern(pat);
            }
        }
        Statement::CreateAndReturn(cr) => {
            for pat in &cr.patterns {
                collect_pattern(pat);
            }
        }
        Statement::Set(s) => {
            for mc in &s.match_clauses {
                collect_pattern(&mc.pattern);
            }
        }
        Statement::SetAndReturn(sr) => {
            for mc in &sr.match_clauses {
                collect_pattern(&mc.pattern);
            }
        }
        Statement::Delete(d) => {
            for mc in &d.match_clauses {
                collect_pattern(&mc.pattern);
            }
        }
        Statement::DeleteAndReturn(dr) => {
            for mc in &dr.match_clauses {
                collect_pattern(&mc.pattern);
            }
        }
        Statement::Merge(m) => {
            collect_pattern(&m.pattern);
        }
        Statement::MergeAndReturn(mr) => {
            for m in &mr.merges {
                collect_pattern(&m.pattern);
            }
        }
        Statement::Remove(r) => {
            for mc in &r.match_clauses {
                collect_pattern(&mc.pattern);
            }
        }
        Statement::RemoveAndReturn(rr) => {
            for mc in &rr.match_clauses {
                collect_pattern(&mc.pattern);
            }
        }
        Statement::Foreach(f) => {
            for s in &f.body {
                collect_node_and_rel_vars_in_stmt(s, node_vars, rel_vars);
            }
        }
        Statement::Union(u) => {
            collect_node_and_rel_vars_in_stmt(&u.left, node_vars, rel_vars);
            collect_node_and_rel_vars_in_stmt(&u.right, node_vars, rel_vars);
        }
        Statement::Pipeline(stmts) => {
            for s in stmts {
                collect_node_and_rel_vars_in_stmt(s, node_vars, rel_vars);
            }
        }
        _ => {}
    }
}

fn collect_path_vars_in_stmt(stmt: &Statement, vars: &mut std::collections::HashSet<String>) {
    match stmt {
        Statement::Query(q) => {
            for mc in &q.match_clauses {
                if let Some(ref pv) = mc.pattern.path_variable {
                    vars.insert(pv.clone());
                }
            }
            for part in &q.parts {
                match part {
                    QueryPart::Match { match_clauses, .. }
                    | QueryPart::OptionalMatch { match_clauses, .. } => {
                        for mc in match_clauses {
                            if let Some(ref pv) = mc.pattern.path_variable {
                                vars.insert(pv.clone());
                            }
                        }
                    }
                    QueryPart::Create { patterns } => {
                        for pat in patterns {
                            if let Some(ref pv) = pat.path_variable {
                                vars.insert(pv.clone());
                            }
                        }
                    }
                    QueryPart::Merge { merges } => {
                        for m in merges {
                            if let Some(ref pv) = m.pattern.path_variable {
                                vars.insert(pv.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Statement::Create(c) => {
            for pat in &c.patterns {
                if let Some(ref pv) = pat.path_variable {
                    vars.insert(pv.clone());
                }
            }
        }
        Statement::Set(s) => {
            for mc in &s.match_clauses {
                if let Some(ref pv) = mc.pattern.path_variable {
                    vars.insert(pv.clone());
                }
            }
        }
        Statement::Delete(d) => {
            for mc in &d.match_clauses {
                if let Some(ref pv) = mc.pattern.path_variable {
                    vars.insert(pv.clone());
                }
            }
        }
        Statement::Merge(m) => {
            if let Some(ref pv) = m.pattern.path_variable {
                vars.insert(pv.clone());
            }
        }
        Statement::Remove(r) => {
            for mc in &r.match_clauses {
                if let Some(ref pv) = mc.pattern.path_variable {
                    vars.insert(pv.clone());
                }
            }
        }
        Statement::Union(u) => {
            collect_path_vars_in_stmt(&u.left, vars);
            collect_path_vars_in_stmt(&u.right, vars);
        }
        Statement::CreateAndReturn(cr) => {
            for pat in &cr.patterns {
                if let Some(ref pv) = pat.path_variable {
                    vars.insert(pv.clone());
                }
            }
        }
        Statement::SetAndReturn(sr) => {
            for mc in &sr.match_clauses {
                if let Some(ref pv) = mc.pattern.path_variable {
                    vars.insert(pv.clone());
                }
            }
        }
        Statement::MergeAndReturn(mr) => {
            for m in &mr.merges {
                if let Some(ref pv) = m.pattern.path_variable {
                    vars.insert(pv.clone());
                }
            }
        }
        Statement::DeleteAndReturn(dr) => {
            for mc in &dr.match_clauses {
                if let Some(ref pv) = mc.pattern.path_variable {
                    vars.insert(pv.clone());
                }
            }
        }
        Statement::RemoveAndReturn(rr) => {
            for mc in &rr.match_clauses {
                if let Some(ref pv) = mc.pattern.path_variable {
                    vars.insert(pv.clone());
                }
            }
        }
        Statement::Foreach(f) => {
            for s in &f.body {
                collect_path_vars_in_stmt(s, vars);
            }
        }
        Statement::Pipeline(stmts) => {
            for s in stmts {
                collect_path_vars_in_stmt(s, vars);
            }
        }
        _ => {}
    }
}

/// True for the six binary comparison operators (`=`, `<>`, `<`, `>`, `<=`, `>=`).
/// Used to decide whether a parenthesized expression needs the `__grouped__` wrapper
/// that blocks chained-comparison desugaring.
fn is_comparison_operator(op: &BinaryOperator) -> bool {
    matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::Ne
            | BinaryOperator::Lt
            | BinaryOperator::Gt
            | BinaryOperator::Le
            | BinaryOperator::Ge
    )
}

fn has_aggregation(expr: &Expr) -> bool {
    match expr {
        Expr::CountStar | Expr::Agg(_, _) => true,
        Expr::Quantifier {
            list, predicate, ..
        } => has_aggregation(list) || has_aggregation(predicate),
        Expr::FunctionCall { args, .. } => args.iter().any(has_aggregation),
        Expr::BinaryOp { left, right, .. } => has_aggregation(left) || has_aggregation(right),
        Expr::IsNull(inner) | Expr::IsNotNull(inner) | Expr::Not(inner) => has_aggregation(inner),
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            subject.as_ref().is_some_and(|s| has_aggregation(s))
                || arms
                    .iter()
                    .any(|a| has_aggregation(&a.when) || has_aggregation(&a.then))
                || else_expr.as_ref().is_some_and(|e| has_aggregation(e))
        }
        Expr::Subscript { expr, index } => has_aggregation(expr) || has_aggregation(index),
        Expr::Slice { expr, start, end } => {
            has_aggregation(expr)
                || start.as_ref().is_some_and(|s| has_aggregation(s))
                || end.as_ref().is_some_and(|e| has_aggregation(e))
        }
        Expr::ListComprehension {
            list,
            predicate,
            transform,
            ..
        } => {
            has_aggregation(list)
                || predicate.as_ref().is_some_and(|p| has_aggregation(p))
                || transform.as_ref().is_some_and(|t| has_aggregation(t))
        }
        Expr::Reduce {
            initial,
            list,
            expression,
            ..
        } => has_aggregation(initial) || has_aggregation(list) || has_aggregation(expression),
        _ => false,
    }
}

fn is_known_function(name: &str) -> bool {
    matches!(
        name,
        "__grouped__"
            | "__list__"
            | "__path__"
            | "__map__"
            | "__star__"
            | "__neg_min_int__"
            | "__in__"
            | "__contains__"
            | "__starts_with__"
            | "__ends_with__"
            | "__regex__"
            | "range"
            | "size"
            | "type"
            | "id"
            | "labels"
            | "length"
            | "substring"
            | "trim"
            | "ltrim"
            | "rtrim"
            | "properties"
            | "startnode"
            | "endnode"
            | "isnull"
            | "isnotnull"
            | "exists"
            | "left"
            | "right"
            | "coalesce"
            | "tostring"
            | "tointeger"
            | "toint"
            | "tofloat"
            | "toboolean"
            | "keys"
            | "head"
            | "last"
            | "tail"
            | "timestamp"
            | "max"
            | "min"
            | "abs"
            | "sqrt"
            | "floor"
            | "ceil"
            | "ceiling"
            | "round"
            | "sign"
            | "log"
            | "log10"
            | "exp"
            | "sin"
            | "cos"
            | "tan"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "pi"
            | "e"
            | "rand"
            | "degrees"
            | "radians"
            | "haversin"
            | "date"
            | "localtime"
            | "time"
            | "localdatetime"
            | "datetime"
            | "duration"
            | "datetime.fromepoch"
            | "datetime.fromepochmillis"
            | "date.truncate"
            | "datetime.truncate"
            | "localdatetime.truncate"
            | "localtime.truncate"
            | "time.truncate"
            | "duration.between"
            | "duration.indays"
            | "duration.inmonths"
            | "duration.inseconds"
            | "date.transaction"
            | "date.statement"
            | "date.realtime"
            | "datetime.transaction"
            | "datetime.statement"
            | "datetime.realtime"
            | "localtime.transaction"
            | "localtime.statement"
            | "localtime.realtime"
            | "localdatetime.transaction"
            | "localdatetime.statement"
            | "localdatetime.realtime"
            | "time.transaction"
            | "time.statement"
            | "time.realtime"
            | "split"
            | "reverse"
            | "replace"
            | "toupper"
            | "tolower"
            | "nodes"
            | "relationships"
            | "rels"
            | "vector_dist"
    )
}

fn check_expr_size_on_path(
    expr: &Expr,
    path_vars: &std::collections::HashSet<String>,
    node_vars: &std::collections::HashSet<String>,
    rel_vars: &std::collections::HashSet<String>,
) -> Result<(), String> {
    match expr {
        Expr::FunctionCall { name, args } => {
            // A surviving 2^63 marker is a standalone positive literal out of the
            // i64 range (it is only valid when negated, which rewrites the marker).
            if name == "__neg_min_int__" {
                return Err(
                    "SyntaxError(IntegerOverflow): integer literal out of range".to_string()
                );
            }
            let name_lc = name.to_ascii_lowercase();
            if !is_known_function(&name_lc) {
                return Err(format!(
                    "SyntaxError(UnknownFunction): unknown function: {}",
                    name
                ));
            }
            let name_lc = name.to_lowercase();
            if name_lc == "size" && args.len() == 1 {
                if let Expr::Prop(var, prop) = &args[0] {
                    if prop.is_empty() && path_vars.contains(var) {
                        return Err(format!(
                            "TypeError: size() cannot be applied to path variable '{}'",
                            var
                        ));
                    }
                }
            } else if name_lc == "length" && args.len() == 1 {
                if let Expr::Prop(var, prop) = &args[0] {
                    if prop.is_empty() && (node_vars.contains(var) || rel_vars.contains(var)) {
                        return Err(format!(
                            "SyntaxError(InvalidArgumentType): length() cannot be applied to node or relationship variable '{}'",
                            var
                        ));
                    }
                }
            } else if name_lc == "type" && args.len() == 1 {
                if let Expr::Prop(var, prop) = &args[0] {
                    if prop.is_empty() && (node_vars.contains(var) || path_vars.contains(var)) {
                        return Err(format!(
                            "SyntaxError(InvalidArgumentType): type() requires a relationship, but '{}' is not",
                            var
                        ));
                    }
                }
            } else if name_lc == "labels" && args.len() == 1 {
                if let Expr::Prop(var, prop) = &args[0] {
                    if prop.is_empty() && (rel_vars.contains(var) || path_vars.contains(var)) {
                        return Err(format!(
                            "SyntaxError(InvalidArgumentType): labels() requires a node, but '{}' is not",
                            var
                        ));
                    }
                }
            }
            for a in args {
                check_expr_size_on_path(a, path_vars, node_vars, rel_vars)?;
            }
        }
        Expr::Agg(_, inner) | Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            check_expr_size_on_path(inner, path_vars, node_vars, rel_vars)?;
        }
        Expr::BinaryOp { left, right, .. } => {
            check_expr_size_on_path(left, path_vars, node_vars, rel_vars)?;
            check_expr_size_on_path(right, path_vars, node_vars, rel_vars)?;
        }
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            if let Some(s) = subject {
                check_expr_size_on_path(s, path_vars, node_vars, rel_vars)?;
            }
            for a in arms {
                check_expr_size_on_path(&a.when, path_vars, node_vars, rel_vars)?;
                check_expr_size_on_path(&a.then, path_vars, node_vars, rel_vars)?;
            }
            if let Some(e) = else_expr {
                check_expr_size_on_path(e, path_vars, node_vars, rel_vars)?;
            }
        }
        Expr::Subscript { expr: e, index } => {
            check_expr_size_on_path(e, path_vars, node_vars, rel_vars)?;
            check_expr_size_on_path(index, path_vars, node_vars, rel_vars)?;
        }
        Expr::Slice {
            expr: e,
            start,
            end,
        } => {
            check_expr_size_on_path(e, path_vars, node_vars, rel_vars)?;
            if let Some(s) = start {
                check_expr_size_on_path(s, path_vars, node_vars, rel_vars)?;
            }
            if let Some(ed) = end {
                check_expr_size_on_path(ed, path_vars, node_vars, rel_vars)?;
            }
        }
        Expr::ListComprehension {
            list,
            predicate,
            transform,
            ..
        } => {
            if predicate.as_ref().is_some_and(|p| has_aggregation(p))
                || transform.as_ref().is_some_and(|t| has_aggregation(t))
            {
                return Err(
                    "TypeError: list comprehension cannot contain aggregation functions"
                        .to_string(),
                );
            }
            check_expr_size_on_path(list, path_vars, node_vars, rel_vars)?;
            if let Some(p) = predicate {
                check_expr_size_on_path(p, path_vars, node_vars, rel_vars)?;
            }
            if let Some(t) = transform {
                check_expr_size_on_path(t, path_vars, node_vars, rel_vars)?;
            }
        }
        Expr::Reduce {
            initial,
            list,
            expression,
            ..
        } => {
            if has_aggregation(initial) || has_aggregation(list) || has_aggregation(expression) {
                return Err("TypeError: reduce() cannot contain aggregation functions".to_string());
            }
            check_expr_size_on_path(initial, path_vars, node_vars, rel_vars)?;
            check_expr_size_on_path(list, path_vars, node_vars, rel_vars)?;
            check_expr_size_on_path(expression, path_vars, node_vars, rel_vars)?;
        }
        _ => {}
    }
    Ok(())
}

fn check_where_clause(
    wc: &WhereClause,
    path_vars: &std::collections::HashSet<String>,
    node_vars: &std::collections::HashSet<String>,
    rel_vars: &std::collections::HashSet<String>,
) -> Result<(), String> {
    match wc {
        WhereClause::Eq(l, r)
        | WhereClause::Ne(l, r)
        | WhereClause::Lt(l, r)
        | WhereClause::Gt(l, r)
        | WhereClause::Le(l, r)
        | WhereClause::Ge(l, r) => {
            check_expr_size_on_path(l, path_vars, node_vars, rel_vars)?;
            check_expr_size_on_path(r, path_vars, node_vars, rel_vars)?;
        }
        WhereClause::Expr(e) => {
            check_expr_size_on_path(e, path_vars, node_vars, rel_vars)?;
        }
    }
    Ok(())
}

fn check_statement_exprs(
    stmt: &Statement,
    path_vars: &std::collections::HashSet<String>,
    node_vars: &std::collections::HashSet<String>,
    rel_vars: &std::collections::HashSet<String>,
) -> Result<(), String> {
    match stmt {
        Statement::Query(q) => {
            if let Some(wc) = &q.where_clause {
                check_where_clause(wc, path_vars, node_vars, rel_vars)?;
            }
            for ri in &q.return_clause.items {
                check_expr_size_on_path(&ri.expr, path_vars, node_vars, rel_vars)?;
            }
            if let Some(ob) = &q.order_by {
                for si in &ob.items {
                    check_expr_size_on_path(&si.expr, path_vars, node_vars, rel_vars)?;
                }
            }
            if let Some(e) = &q.skip {
                check_expr_size_on_path(e, path_vars, node_vars, rel_vars)?;
            }
            if let Some(e) = &q.limit {
                check_expr_size_on_path(e, path_vars, node_vars, rel_vars)?;
            }
            for part in &q.parts {
                check_query_part_exprs(part, path_vars, node_vars, rel_vars)?;
            }
        }
        Statement::Set(s) => {
            for part in &s.match_clauses {
                if let Some(ref props) = part.pattern.node.properties {
                    for v in props.values() {
                        check_expr_size_on_path(v, path_vars, node_vars, rel_vars)?;
                    }
                }
                for (rel, target) in &part.pattern.rels {
                    if let Some(ref props) = rel.properties {
                        for v in props.values() {
                            check_expr_size_on_path(v, path_vars, node_vars, rel_vars)?;
                        }
                    }
                    if let Some(ref props) = target.properties {
                        for v in props.values() {
                            check_expr_size_on_path(v, path_vars, node_vars, rel_vars)?;
                        }
                    }
                }
            }
        }
        Statement::Delete(_) | Statement::Remove(_) => {}
        Statement::Union(u) => {
            check_statement_exprs(&u.left, path_vars, node_vars, rel_vars)?;
            check_statement_exprs(&u.right, path_vars, node_vars, rel_vars)?;
        }
        Statement::SetAndReturn(sr) => {
            for ri in &sr.return_clause.items {
                check_expr_size_on_path(&ri.expr, path_vars, node_vars, rel_vars)?;
            }
        }
        Statement::DeleteAndReturn(dr) => {
            for ri in &dr.return_clause.items {
                check_expr_size_on_path(&ri.expr, path_vars, node_vars, rel_vars)?;
            }
        }
        Statement::RemoveAndReturn(rr) => {
            for ri in &rr.return_clause.items {
                check_expr_size_on_path(&ri.expr, path_vars, node_vars, rel_vars)?;
            }
        }
        Statement::Foreach(f) => {
            check_expr_size_on_path(&f.list, path_vars, node_vars, rel_vars)?;
            for s in &f.body {
                check_statement_exprs(s, path_vars, node_vars, rel_vars)?;
            }
        }
        Statement::Pipeline(stmts) => {
            for s in stmts {
                check_statement_exprs(s, path_vars, node_vars, rel_vars)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_query_part_exprs(
    part: &QueryPart,
    path_vars: &std::collections::HashSet<String>,
    node_vars: &std::collections::HashSet<String>,
    rel_vars: &std::collections::HashSet<String>,
) -> Result<(), String> {
    match part {
        QueryPart::Match { where_clause, .. } | QueryPart::OptionalMatch { where_clause, .. } => {
            if let Some(wc) = where_clause {
                check_where_clause(wc, path_vars, node_vars, rel_vars)?;
            }
        }
        QueryPart::With {
            items,
            where_clause,
            order_by,
            skip,
            limit,
            ..
        } => {
            for ri in items {
                check_expr_size_on_path(&ri.expr, path_vars, node_vars, rel_vars)?;
            }
            if let Some(wc) = where_clause {
                check_where_clause(wc, path_vars, node_vars, rel_vars)?;
            }
            if let Some(ob) = order_by {
                for si in &ob.items {
                    check_expr_size_on_path(&si.expr, path_vars, node_vars, rel_vars)?;
                }
            }
            if let Some(e) = skip {
                check_expr_size_on_path(e, path_vars, node_vars, rel_vars)?;
            }
            if let Some(e) = limit {
                check_expr_size_on_path(e, path_vars, node_vars, rel_vars)?;
            }
        }
        QueryPart::Unwind { expr, .. } => {
            check_expr_size_on_path(expr, path_vars, node_vars, rel_vars)?;
        }
        QueryPart::Set { items } => {
            for si in items {
                if let SetItem::Property { expr, .. } = si {
                    check_expr_size_on_path(expr, path_vars, node_vars, rel_vars)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_where_vars(w: &WhereClause, vars: &mut std::collections::HashSet<String>) {
    match w {
        WhereClause::Eq(l, r)
        | WhereClause::Ne(l, r)
        | WhereClause::Lt(l, r)
        | WhereClause::Gt(l, r)
        | WhereClause::Le(l, r)
        | WhereClause::Ge(l, r) => {
            collect_expr_vars(l, vars);
            collect_expr_vars(r, vars);
        }
        WhereClause::Expr(e) => {
            collect_expr_vars(e, vars);
        }
    }
}

fn collect_pattern_bound_vars(pattern: &Pattern, bound: &mut std::collections::HashSet<String>) {
    if let Some(ref pv) = pattern.path_variable {
        bound.insert(pv.clone());
    }
    if let Some(ref v) = pattern.node.variable {
        bound.insert(v.clone());
    }
    for (rel, target) in &pattern.rels {
        if let Some(ref v) = rel.variable {
            bound.insert(v.clone());
        }
        if let Some(ref v) = target.variable {
            bound.insert(v.clone());
        }
    }
}

fn validate_expr_vars(
    expr: &Expr,
    active: &std::collections::HashSet<String>,
) -> Result<(), String> {
    let mut vars = std::collections::HashSet::new();
    collect_expr_vars(expr, &mut vars);
    for v in vars {
        if !active.contains(&v) {
            return Err(format!(
                "SyntaxError(UndefinedVariable): variable '{}' is not in scope",
                v
            ));
        }
    }
    Ok(())
}

/// Validate that a SET item's target variable is in scope, and (for a property
/// assignment) that its value expression references only in-scope variables.
fn validate_set_item_vars(
    item: &SetItem,
    active: &std::collections::HashSet<String>,
) -> Result<(), String> {
    if !active.contains(item.variable()) {
        return Err(format!(
            "SyntaxError(UndefinedVariable): variable '{}' is not in scope",
            item.variable()
        ));
    }
    if let SetItem::Property { expr, .. } = item {
        validate_expr_vars(expr, active)?;
    }
    Ok(())
}

fn validate_where_vars(
    w: &WhereClause,
    active: &std::collections::HashSet<String>,
) -> Result<(), String> {
    let mut vars = std::collections::HashSet::new();
    collect_where_vars(w, &mut vars);
    for v in vars {
        if !active.contains(&v) {
            return Err(format!(
                "SyntaxError(UndefinedVariable): variable '{}' is not in scope",
                v
            ));
        }
    }
    Ok(())
}

fn validate_pattern_exprs(
    pattern: &Pattern,
    active: &std::collections::HashSet<String>,
) -> Result<(), String> {
    if let Some(ref props) = pattern.node.properties {
        for expr in props.values() {
            validate_expr_vars(expr, active)?;
        }
    }
    for (rel, target) in &pattern.rels {
        if let Some(ref props) = rel.properties {
            for expr in props.values() {
                validate_expr_vars(expr, active)?;
            }
        }
        if let Some(ref props) = target.properties {
            for expr in props.values() {
                validate_expr_vars(expr, active)?;
            }
        }
    }
    Ok(())
}

fn validate_statement_undefined_vars_impl(
    stmt: &Statement,
    active: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    match stmt {
        Statement::Query(q) => {
            // Match clauses at top-level
            for mc in &q.match_clauses {
                validate_pattern_exprs(&mc.pattern, active)?;
                collect_pattern_bound_vars(&mc.pattern, active);
            }
            if let Some(wc) = &q.where_clause {
                validate_where_vars(wc, active)?;
            }
            // Parts
            for part in &q.parts {
                match part {
                    QueryPart::Match {
                        match_clauses,
                        where_clause,
                    }
                    | QueryPart::OptionalMatch {
                        match_clauses,
                        where_clause,
                    } => {
                        for mc in match_clauses {
                            validate_pattern_exprs(&mc.pattern, active)?;
                            collect_pattern_bound_vars(&mc.pattern, active);
                        }
                        if let Some(wc) = where_clause {
                            validate_where_vars(wc, active)?;
                        }
                    }
                    QueryPart::With {
                        items,
                        where_clause,
                        order_by,
                        skip,
                        limit,
                        ..
                    } => {
                        for item in items {
                            validate_expr_vars(&item.expr, active)?;
                        }
                        let mut with_active = active.clone();
                        for item in items {
                            if let Some(ref alias) = item.alias {
                                with_active.insert(alias.clone());
                            } else if let Expr::Prop(ref v, ref p) = item.expr {
                                if p.is_empty() {
                                    with_active.insert(v.clone());
                                }
                            }
                        }
                        if let Some(wc) = where_clause {
                            validate_where_vars(wc, &with_active)?;
                        }
                        if let Some(ob) = order_by {
                            for si in &ob.items {
                                validate_expr_vars(&si.expr, &with_active)?;
                            }
                        }
                        if let Some(s) = skip {
                            validate_expr_vars(s, active)?;
                        }
                        if let Some(l) = limit {
                            validate_expr_vars(l, active)?;
                        }
                        if let Some(new_scope) = with_output_scope(items) {
                            *active = new_scope;
                        }
                    }
                    QueryPart::Unwind { expr, variable } => {
                        validate_expr_vars(expr, active)?;
                        active.insert(variable.clone());
                    }
                    QueryPart::Create { patterns } => {
                        for p in patterns {
                            validate_pattern_exprs(p, active)?;
                            collect_pattern_bound_vars(p, active);
                        }
                    }
                    QueryPart::Merge { merges } => {
                        for m in merges {
                            validate_pattern_exprs(&m.pattern, active)?;
                            collect_pattern_bound_vars(&m.pattern, active);
                            for item in &m.on_create_set {
                                validate_set_item_vars(item, active)?;
                            }
                            for item in &m.on_match_set {
                                validate_set_item_vars(item, active)?;
                            }
                        }
                    }
                    QueryPart::Set { items } => {
                        for item in items {
                            validate_set_item_vars(item, active)?;
                        }
                    }
                    QueryPart::Delete { targets, .. } => {
                        for t in targets {
                            validate_expr_vars(t, active)?;
                        }
                    }
                    QueryPart::Remove { items } => {
                        for item in items {
                            let v = match item {
                                RemoveItem::Property { variable, .. } => variable,
                                RemoveItem::Label { variable, .. } => variable,
                            };
                            if !active.contains(v) {
                                return Err(format!(
                                    "SyntaxError(UndefinedVariable): variable '{}' is not in scope",
                                    v
                                ));
                            }
                        }
                    }
                    QueryPart::Call { args, yields, .. } => {
                        for arg in args {
                            validate_expr_vars(arg, active)?;
                        }
                        // YIELD introduces the (optionally renamed) output fields
                        // into scope for following clauses.
                        if let Some(items) = yields {
                            for (field, alias) in items {
                                active.insert(alias.clone().unwrap_or_else(|| field.clone()));
                            }
                        }
                    }
                }
            }
            // Return clause, order by, skip, limit
            for item in &q.return_clause.items {
                validate_expr_vars(&item.expr, active)?;
            }
            if let Some(ob) = &q.order_by {
                // ORDER BY sees the RETURN projection scope in addition to the
                // match-bound variables, so an alias introduced by RETURN is a valid sort key.
                let mut return_active = active.clone();
                for item in &q.return_clause.items {
                    if let Some(ref alias) = item.alias {
                        return_active.insert(alias.clone());
                    }
                }
                for si in &ob.items {
                    validate_expr_vars(&si.expr, &return_active)?;
                }
            }
            if let Some(s) = &q.skip {
                validate_expr_vars(s, active)?;
            }
            if let Some(l) = &q.limit {
                validate_expr_vars(l, active)?;
            }
        }
        Statement::Create(c) => {
            for p in &c.patterns {
                validate_pattern_exprs(p, active)?;
                collect_pattern_bound_vars(p, active);
            }
        }
        Statement::Set(s) => {
            for mc in &s.match_clauses {
                validate_pattern_exprs(&mc.pattern, active)?;
                collect_pattern_bound_vars(&mc.pattern, active);
            }
            if let Some(wc) = &s.where_clause {
                validate_where_vars(wc, active)?;
            }
            for item in &s.set_items {
                validate_set_item_vars(item, active)?;
            }
        }
        Statement::Delete(d) => {
            for mc in &d.match_clauses {
                validate_pattern_exprs(&mc.pattern, active)?;
                collect_pattern_bound_vars(&mc.pattern, active);
            }
            if let Some(wc) = &d.where_clause {
                validate_where_vars(wc, active)?;
            }
            for t in &d.targets {
                validate_expr_vars(t, active)?;
            }
        }
        Statement::Remove(r) => {
            for mc in &r.match_clauses {
                validate_pattern_exprs(&mc.pattern, active)?;
                collect_pattern_bound_vars(&mc.pattern, active);
            }
            if let Some(wc) = &r.where_clause {
                validate_where_vars(wc, active)?;
            }
            for item in &r.items {
                let v = match item {
                    RemoveItem::Property { variable, .. } => variable,
                    RemoveItem::Label { variable, .. } => variable,
                };
                if !active.contains(v) {
                    return Err(format!(
                        "SyntaxError(UndefinedVariable): variable '{}' is not in scope",
                        v
                    ));
                }
            }
        }
        Statement::Union(u) => {
            let mut left_active = active.clone();
            validate_statement_undefined_vars_impl(&u.left, &mut left_active)?;
            let mut right_active = active.clone();
            validate_statement_undefined_vars_impl(&u.right, &mut right_active)?;
        }
        Statement::SetAndReturn(sr) => {
            for mc in &sr.match_clauses {
                validate_pattern_exprs(&mc.pattern, active)?;
                collect_pattern_bound_vars(&mc.pattern, active);
            }
            if let Some(wc) = &sr.where_clause {
                validate_where_vars(wc, active)?;
            }
            for item in &sr.set_items {
                validate_set_item_vars(item, active)?;
            }
            for item in &sr.return_clause.items {
                validate_expr_vars(&item.expr, active)?;
            }
        }
        Statement::DeleteAndReturn(dr) => {
            for mc in &dr.match_clauses {
                validate_pattern_exprs(&mc.pattern, active)?;
                collect_pattern_bound_vars(&mc.pattern, active);
            }
            if let Some(wc) = &dr.where_clause {
                validate_where_vars(wc, active)?;
            }
            for t in &dr.targets {
                validate_expr_vars(t, active)?;
            }
            for item in &dr.return_clause.items {
                validate_expr_vars(&item.expr, active)?;
            }
        }
        Statement::RemoveAndReturn(rr) => {
            for mc in &rr.match_clauses {
                validate_pattern_exprs(&mc.pattern, active)?;
                collect_pattern_bound_vars(&mc.pattern, active);
            }
            if let Some(wc) = &rr.where_clause {
                validate_where_vars(wc, active)?;
            }
            for item in &rr.items {
                let v = match item {
                    RemoveItem::Property { variable, .. } => variable,
                    RemoveItem::Label { variable, .. } => variable,
                };
                if !active.contains(v) {
                    return Err(format!(
                        "SyntaxError(UndefinedVariable): variable '{}' is not in scope",
                        v
                    ));
                }
            }
            for item in &rr.return_clause.items {
                validate_expr_vars(&item.expr, active)?;
            }
        }
        Statement::Foreach(f) => {
            validate_expr_vars(&f.list, active)?;
            let mut body_active = active.clone();
            body_active.insert(f.variable.clone());
            for s in &f.body {
                validate_statement_undefined_vars_impl(s, &mut body_active)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_statement_undefined_vars(stmt: &Statement) -> Result<(), String> {
    let mut active = std::collections::HashSet::new();
    validate_statement_undefined_vars_impl(stmt, &mut active)
}

fn validate_statement(stmt: &Statement) -> Result<(), String> {
    validate_statement_undefined_vars(stmt)?;
    validate_create_semantics(stmt)?;
    validate_merge_semantics(stmt)?;
    validate_delete_semantics(stmt)?;
    validate_projection_semantics(stmt)?;
    validate_value_var_conflicts(stmt)?;
    let mut path_vars = std::collections::HashSet::new();
    collect_path_vars_in_stmt(stmt, &mut path_vars);
    let mut node_vars = std::collections::HashSet::new();
    let mut rel_vars = std::collections::HashSet::new();
    collect_node_and_rel_vars_in_stmt(stmt, &mut node_vars, &mut rel_vars);
    check_statement_exprs(stmt, &path_vars, &node_vars, &rel_vars)?;

    match stmt {
        Statement::Query(q) => {
            validate_cross_clause_variable_types(&q.parts)?;
            validate_order_by_scope(&q.parts)?;
            validate_query_order_by(q)?;
            for part in &q.parts {
                match part {
                    QueryPart::Match { match_clauses, .. } => {
                        validate_match_clause_variables(match_clauses)?;
                    }
                    QueryPart::OptionalMatch { match_clauses, .. } => {
                        validate_match_clause_variables(match_clauses)?;
                    }
                    _ => {}
                }
            }
        }
        Statement::Set(s) => {
            validate_match_clause_variables(&s.match_clauses)?;
        }
        Statement::Delete(d) => {
            validate_match_clause_variables(&d.match_clauses)?;
        }
        Statement::Remove(r) => {
            validate_match_clause_variables(&r.match_clauses)?;
        }
        Statement::Union(u) => {
            validate_statement(&u.left)?;
            validate_statement(&u.right)?;
        }
        Statement::SetAndReturn(sr) => {
            validate_match_clause_variables(&sr.match_clauses)?;
        }
        Statement::DeleteAndReturn(dr) => {
            validate_match_clause_variables(&dr.match_clauses)?;
        }
        Statement::RemoveAndReturn(rr) => {
            validate_match_clause_variables(&rr.match_clauses)?;
        }
        Statement::Foreach(f) => {
            for s in &f.body {
                validate_statement(s)?;
            }
        }
        Statement::Pipeline(stmts) => {
            for s in stmts {
                validate_statement(s)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// ─── CREATE clause semantics ────────────────────────────────────────────────

/// Validate CREATE clauses against the openCypher structural rules: a created
/// relationship must have exactly one type, must be directed, and must not be
/// variable-length, and a CREATE pattern must not reuse an already-bound variable
/// (a bound node may appear only as a bare relationship endpoint, never carrying
/// labels or properties, and never standing alone).
fn validate_create_semantics(stmt: &Statement) -> Result<(), String> {
    match stmt {
        Statement::Query(q) => {
            let mut bound = std::collections::HashSet::new();
            for mc in &q.match_clauses {
                collect_pattern_bound_vars(&mc.pattern, &mut bound);
            }
            for part in &q.parts {
                match part {
                    QueryPart::Match { match_clauses, .. }
                    | QueryPart::OptionalMatch { match_clauses, .. } => {
                        for mc in match_clauses {
                            collect_pattern_bound_vars(&mc.pattern, &mut bound);
                        }
                    }
                    QueryPart::With { items, .. } => {
                        // WITH is a scope barrier: only projected aliases survive.
                        bound = with_output_scope(items).unwrap_or_default();
                    }
                    QueryPart::Unwind { variable, .. } => {
                        bound.insert(variable.clone());
                    }
                    QueryPart::Create { patterns } => {
                        validate_create_patterns(patterns, &mut bound)?;
                    }
                    _ => {}
                }
            }
        }
        Statement::Create(c) => {
            let mut bound = std::collections::HashSet::new();
            validate_create_patterns(&c.patterns, &mut bound)?;
        }
        Statement::CreateAndReturn(c) => {
            let mut bound = std::collections::HashSet::new();
            validate_create_patterns(&c.patterns, &mut bound)?;
        }
        Statement::Union(u) => {
            validate_create_semantics(&u.left)?;
            validate_create_semantics(&u.right)?;
        }
        Statement::Pipeline(stmts) => {
            for s in stmts {
                validate_create_semantics(s)?;
            }
        }
        Statement::Foreach(f) => {
            for s in &f.body {
                validate_create_semantics(s)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Validate the patterns of one CREATE clause against the variables bound before
/// it, then add the newly created variables to `bound`. All patterns in the clause
/// are checked against the same pre-clause snapshot so that two standalone node
/// patterns referring to the same fresh variable are not falsely rejected.
fn validate_create_patterns(
    patterns: &[Pattern],
    bound: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    let snapshot = bound.clone();
    for p in patterns {
        let has_rels = !p.rels.is_empty();
        check_create_node(&p.node, &snapshot, has_rels)?;
        for (rel, target) in &p.rels {
            check_create_rel(rel, &snapshot)?;
            check_create_node(target, &snapshot, true)?;
        }
    }
    for p in patterns {
        collect_pattern_bound_vars(p, bound);
    }
    Ok(())
}

/// A created node may reuse an already-bound variable only as a bare relationship
/// endpoint: it must carry no labels or properties, and the surrounding pattern
/// must contain at least one relationship.
fn check_create_node(
    node: &NodePattern,
    bound: &std::collections::HashSet<String>,
    pattern_has_rels: bool,
) -> Result<(), String> {
    if let Some(ref v) = node.variable {
        if bound.contains(v)
            && (!node.labels.is_empty() || node.properties.is_some() || !pattern_has_rels)
        {
            return Err(format!(
                "SyntaxError(VariableAlreadyBound): variable '{}' is already bound",
                v
            ));
        }
    }
    Ok(())
}

/// A created relationship must have exactly one type, be directed, not be
/// variable-length, and not reuse an already-bound variable.
fn check_create_rel(
    rel: &RelationshipPattern,
    bound: &std::collections::HashSet<String>,
) -> Result<(), String> {
    match rel.rel_type {
        None => {
            return Err(
                "SyntaxError(NoSingleRelationshipType): a created relationship must have a single type"
                    .to_string(),
            );
        }
        Some(ref t) if t.contains('|') => {
            return Err(
                "SyntaxError(NoSingleRelationshipType): a created relationship must have a single type"
                    .to_string(),
            );
        }
        _ => {}
    }
    if rel.is_undirected {
        return Err(
            "SyntaxError(RequiresDirectedRelationship): a created relationship must be directed"
                .to_string(),
        );
    }
    if rel.range.is_some() {
        return Err(
            "SyntaxError(CreatingVarLength): cannot create a variable-length relationship"
                .to_string(),
        );
    }
    if let Some(ref v) = rel.variable {
        if bound.contains(v) {
            return Err(format!(
                "SyntaxError(VariableAlreadyBound): relationship variable '{}' is already bound",
                v
            ));
        }
    }
    Ok(())
}

// ─── DELETE clause semantics ────────────────────────────────────────────────

/// Validate DELETE targets: a target must be able to denote a node, relationship,
/// or path. Expressions whose static form can never be a graph element (a literal,
/// an arithmetic or boolean operation, a label check, an aggregation) are rejected
/// at compile time.
fn validate_delete_semantics(stmt: &Statement) -> Result<(), String> {
    match stmt {
        Statement::Query(q) => {
            for part in &q.parts {
                if let QueryPart::Delete { targets, .. } = part {
                    for t in targets {
                        check_delete_target(t)?;
                    }
                }
            }
        }
        Statement::Delete(d) => {
            for t in &d.targets {
                check_delete_target(t)?;
            }
        }
        Statement::DeleteAndReturn(d) => {
            for t in &d.targets {
                check_delete_target(t)?;
            }
        }
        Statement::Union(u) => {
            validate_delete_semantics(&u.left)?;
            validate_delete_semantics(&u.right)?;
        }
        Statement::Pipeline(stmts) => {
            for s in stmts {
                validate_delete_semantics(s)?;
            }
        }
        Statement::Foreach(f) => {
            for s in &f.body {
                validate_delete_semantics(s)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_delete_target(expr: &Expr) -> Result<(), String> {
    match expr {
        Expr::HasLabel { .. } => Err(
            "SyntaxError(InvalidDelete): cannot delete a label; use REMOVE to remove labels"
                .to_string(),
        ),
        Expr::Literal(_)
        | Expr::BinaryOp { .. }
        | Expr::IsNull(_)
        | Expr::IsNotNull(_)
        | Expr::Not(_)
        | Expr::CountStar
        | Expr::Agg(_, _)
        | Expr::Quantifier { .. } => Err(
            "SyntaxError(InvalidArgumentType): DELETE expects a node, relationship, or path"
                .to_string(),
        ),
        _ => Ok(()),
    }
}

// ─── MERGE clause semantics ─────────────────────────────────────────────────

/// Validate MERGE clauses against the openCypher structural rules: a merged
/// relationship must have exactly one type and must not be variable-length, a
/// merged pattern must not reuse an already-bound variable except as a bare
/// relationship endpoint, and a merged node or relationship must not carry a
/// null inline property. Unlike CREATE, MERGE permits an undirected relationship
/// (it is created with outgoing direction when unspecified).
fn validate_merge_semantics(stmt: &Statement) -> Result<(), String> {
    match stmt {
        Statement::Query(q) => {
            let mut bound = std::collections::HashSet::new();
            for mc in &q.match_clauses {
                collect_pattern_bound_vars(&mc.pattern, &mut bound);
            }
            for part in &q.parts {
                match part {
                    QueryPart::Match { match_clauses, .. }
                    | QueryPart::OptionalMatch { match_clauses, .. } => {
                        for mc in match_clauses {
                            collect_pattern_bound_vars(&mc.pattern, &mut bound);
                        }
                    }
                    QueryPart::With { items, .. } => {
                        bound = with_output_scope(items).unwrap_or_default();
                    }
                    QueryPart::Unwind { variable, .. } => {
                        bound.insert(variable.clone());
                    }
                    QueryPart::Create { patterns } => {
                        for p in patterns {
                            collect_pattern_bound_vars(p, &mut bound);
                        }
                    }
                    QueryPart::Merge { merges } => {
                        for m in merges {
                            check_merge_pattern(&m.pattern, &mut bound)?;
                            check_merge_on_clauses(m, &bound)?;
                        }
                    }
                    _ => {}
                }
            }
        }
        Statement::Merge(m) => {
            let mut bound = std::collections::HashSet::new();
            check_merge_pattern(&m.pattern, &mut bound)?;
            check_merge_on_clauses(m, &bound)?;
        }
        Statement::MergeAndReturn(mr) => {
            let mut bound = std::collections::HashSet::new();
            for m in &mr.merges {
                check_merge_pattern(&m.pattern, &mut bound)?;
                check_merge_on_clauses(m, &bound)?;
            }
        }
        Statement::Union(u) => {
            validate_merge_semantics(&u.left)?;
            validate_merge_semantics(&u.right)?;
        }
        Statement::Pipeline(stmts) => {
            for s in stmts {
                validate_merge_semantics(s)?;
            }
        }
        Statement::Foreach(f) => {
            for s in &f.body {
                validate_merge_semantics(s)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_merge_pattern(
    p: &Pattern,
    bound: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    let snapshot = bound.clone();
    let has_rels = !p.rels.is_empty();
    // A bound node may appear only as a bare relationship endpoint; the rule is
    // identical to CREATE.
    check_create_node(&p.node, &snapshot, has_rels)?;
    check_merge_null_props(&p.node.properties)?;
    for (rel, target) in &p.rels {
        check_merge_rel(rel, &snapshot)?;
        check_merge_null_props(&rel.properties)?;
        check_create_node(target, &snapshot, true)?;
        check_merge_null_props(&target.properties)?;
    }
    collect_pattern_bound_vars(p, bound);
    Ok(())
}

/// The target variable of every `ON CREATE SET` and `ON MATCH SET` action must be
/// a variable bound by the surrounding query or by the MERGE pattern itself.
fn check_merge_on_clauses(
    m: &MergeStatement,
    available: &std::collections::HashSet<String>,
) -> Result<(), String> {
    for item in m.on_create_set.iter().chain(m.on_match_set.iter()) {
        let v = item.variable();
        if !available.contains(v) {
            return Err(format!(
                "SyntaxError(UndefinedVariable): variable '{}' not defined",
                v
            ));
        }
    }
    Ok(())
}

/// A merged relationship must have exactly one type, must not be variable-length,
/// and must not reuse an already-bound variable. Undirected is allowed.
fn check_merge_rel(
    rel: &RelationshipPattern,
    bound: &std::collections::HashSet<String>,
) -> Result<(), String> {
    match rel.rel_type {
        None => {
            return Err(
                "SyntaxError(NoSingleRelationshipType): a merged relationship must have a single type"
                    .to_string(),
            );
        }
        Some(ref t) if t.contains('|') => {
            return Err(
                "SyntaxError(NoSingleRelationshipType): a merged relationship must have a single type"
                    .to_string(),
            );
        }
        _ => {}
    }
    if rel.range.is_some() {
        return Err(
            "SyntaxError(CreatingVarLength): cannot merge a variable-length relationship"
                .to_string(),
        );
    }
    if let Some(ref v) = rel.variable {
        if bound.contains(v) {
            return Err(format!(
                "SyntaxError(VariableAlreadyBound): relationship variable '{}' is already bound",
                v
            ));
        }
    }
    Ok(())
}

/// Reject a literal null among a merged element's inline properties. Merging on a
/// null property value is a semantic error in openCypher.
fn check_merge_null_props(
    props: &Option<std::collections::HashMap<String, Expr>>,
) -> Result<(), String> {
    if let Some(map) = props {
        for value in map.values() {
            if matches!(value, Expr::Literal(Literal::Null)) {
                return Err(
                    "SyntaxError(InvalidArgumentValue): cannot merge on a null property value"
                        .to_string(),
                );
            }
        }
    }
    Ok(())
}

// ─── RETURN and aggregation semantics ─────────────────────────────────────────

/// Validate projection lists across the statement: no duplicate output column
/// names, no aggregation nested inside another aggregation, and no
/// non-deterministic function (`rand()`) inside an aggregation.
fn validate_projection_semantics(stmt: &Statement) -> Result<(), String> {
    for items in projection_lists(stmt) {
        check_duplicate_columns(items)?;
        for item in items {
            check_aggregation_arg(&item.expr)?;
        }
        check_ambiguous_aggregations(items)?;
    }
    check_with_aliasing(stmt)?;
    check_no_aggregate_in_where(stmt)?;
    Ok(())
}

fn check_ambiguous_aggregations(items: &[ReturnItem]) -> Result<(), String> {
    let has_agg = items.iter().any(|item| expr_contains_aggregate(&item.expr));
    if !has_agg {
        return Ok(());
    }

    let mut grouping_exprs = Vec::new();
    let mut grouping_aliases = std::collections::HashSet::new();

    for item in items {
        if !expr_contains_aggregate(&item.expr) {
            grouping_exprs.push(&item.expr);
            if let Some(alias) = &item.alias {
                grouping_aliases.insert(alias.clone());
            } else if let Expr::Prop(v, p) = &item.expr {
                if p.is_empty() {
                    grouping_aliases.insert(v.clone());
                }
            }
        }
    }

    let local_vars = std::collections::HashSet::new();
    for item in items {
        if expr_contains_aggregate(&item.expr) {
            check_expr_non_agg(&item.expr, &grouping_exprs, &grouping_aliases, &local_vars)?;
        }
    }

    Ok(())
}

fn check_expr_non_agg(
    expr: &Expr,
    grouping_exprs: &[&Expr],
    grouping_aliases: &std::collections::HashSet<String>,
    local_vars: &std::collections::HashSet<String>,
) -> Result<(), String> {
    if matches!(expr, Expr::CountStar) || matches!(expr, Expr::Agg(_, _)) {
        return Ok(());
    }

    if matches!(expr, Expr::Literal(_) | Expr::Param(_)) {
        return Ok(());
    }

    if matches!(expr, Expr::Prop(_, _)) && grouping_exprs.iter().any(|&ge| ge == expr) {
        return Ok(());
    }

    if let Expr::Prop(v, p) = expr {
        if p.is_empty() && grouping_aliases.contains(v) {
            return Ok(());
        }
        if local_vars.contains(v) {
            return Ok(());
        }
    }

    if let Expr::Prop(v, _) = expr {
        return Err(format!(
            "SyntaxError(AmbiguousAggregationExpression): variable '{}' is not a grouping key",
            v
        ));
    }

    match expr {
        Expr::HasLabel { variable, .. } => {
            if !grouping_aliases.contains(variable) && !local_vars.contains(variable) {
                return Err(format!(
                    "SyntaxError(AmbiguousAggregationExpression): variable '{}' is not a grouping key",
                    variable
                ));
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            check_expr_non_agg(left, grouping_exprs, grouping_aliases, local_vars)?;
            check_expr_non_agg(right, grouping_exprs, grouping_aliases, local_vars)?;
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                check_expr_non_agg(arg, grouping_exprs, grouping_aliases, local_vars)?;
            }
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            check_expr_non_agg(inner, grouping_exprs, grouping_aliases, local_vars)?;
        }
        Expr::Subscript { expr, index } => {
            check_expr_non_agg(expr, grouping_exprs, grouping_aliases, local_vars)?;
            check_expr_non_agg(index, grouping_exprs, grouping_aliases, local_vars)?;
        }
        Expr::Slice { expr, start, end } => {
            check_expr_non_agg(expr, grouping_exprs, grouping_aliases, local_vars)?;
            if let Some(s) = start {
                check_expr_non_agg(s, grouping_exprs, grouping_aliases, local_vars)?;
            }
            if let Some(e) = end {
                check_expr_non_agg(e, grouping_exprs, grouping_aliases, local_vars)?;
            }
        }
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            transform,
        } => {
            check_expr_non_agg(list, grouping_exprs, grouping_aliases, local_vars)?;
            let mut local_vars = local_vars.clone();
            local_vars.insert(variable.clone());
            if let Some(p) = predicate {
                check_expr_non_agg(p, grouping_exprs, grouping_aliases, &local_vars)?;
            }
            if let Some(t) = transform {
                check_expr_non_agg(t, grouping_exprs, grouping_aliases, &local_vars)?;
            }
        }
        Expr::PatternComprehension {
            pattern,
            predicate,
            transform,
        } => {
            let mut local_vars = local_vars.clone();
            if let Some(ref pv) = pattern.path_variable {
                local_vars.insert(pv.clone());
            }
            if let Some(ref nv) = pattern.node.variable {
                local_vars.insert(nv.clone());
            }
            for (rel, node) in &pattern.rels {
                if let Some(ref nv) = node.variable {
                    local_vars.insert(nv.clone());
                }
                if let Some(ref rv) = rel.variable {
                    local_vars.insert(rv.clone());
                }
            }
            if let Some(p) = predicate {
                check_expr_non_agg(p, grouping_exprs, grouping_aliases, &local_vars)?;
            }
            check_expr_non_agg(transform, grouping_exprs, grouping_aliases, &local_vars)?;
        }
        Expr::Reduce {
            accumulator,
            initial,
            variable,
            list,
            expression,
        } => {
            check_expr_non_agg(initial, grouping_exprs, grouping_aliases, local_vars)?;
            check_expr_non_agg(list, grouping_exprs, grouping_aliases, local_vars)?;
            let mut local_vars = local_vars.clone();
            local_vars.insert(accumulator.clone());
            local_vars.insert(variable.clone());
            check_expr_non_agg(expression, grouping_exprs, grouping_aliases, &local_vars)?;
        }
        Expr::Quantifier {
            variable,
            list,
            predicate,
            ..
        } => {
            check_expr_non_agg(list, grouping_exprs, grouping_aliases, local_vars)?;
            let mut local_vars = local_vars.clone();
            local_vars.insert(variable.clone());
            check_expr_non_agg(predicate, grouping_exprs, grouping_aliases, &local_vars)?;
        }
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            if let Some(s) = subject {
                check_expr_non_agg(s, grouping_exprs, grouping_aliases, local_vars)?;
            }
            for arm in arms {
                check_expr_non_agg(&arm.when, grouping_exprs, grouping_aliases, local_vars)?;
                check_expr_non_agg(&arm.then, grouping_exprs, grouping_aliases, local_vars)?;
            }
            if let Some(e) = else_expr {
                check_expr_non_agg(e, grouping_exprs, grouping_aliases, local_vars)?;
            }
        }
        _ => {}
    }

    Ok(())
}

/// Every WITH item that is not a bare variable reference must be aliased. A
/// projected expression without a name cannot be referenced downstream, so
/// openCypher rejects it.
fn check_with_aliasing(stmt: &Statement) -> Result<(), String> {
    let check = |items: &[ReturnItem]| -> Result<(), String> {
        for item in items {
            let is_bare_var = matches!(&item.expr, Expr::Prop(_, p) if p.is_empty());
            // `WITH *` is parsed as a `__star__` sentinel; it needs no alias.
            let is_star =
                matches!(&item.expr, Expr::FunctionCall { name, .. } if name == "__star__");
            if item.alias.is_none() && !is_bare_var && !is_star {
                return Err(
                    "SyntaxError(NoExpressionAlias): expression in WITH must be aliased"
                        .to_string(),
                );
            }
        }
        Ok(())
    };
    match stmt {
        Statement::Query(q) => {
            for part in &q.parts {
                if let QueryPart::With { items, .. } = part {
                    check(items)?;
                }
            }
        }
        Statement::Union(u) => {
            check_with_aliasing(&u.left)?;
            check_with_aliasing(&u.right)?;
        }
        Statement::Pipeline(stmts) => {
            for s in stmts {
                check_with_aliasing(s)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// An aggregating function may not appear in a WHERE predicate.
fn check_no_aggregate_in_where(stmt: &Statement) -> Result<(), String> {
    let check_where = |wc: &WhereClause| -> Result<(), String> {
        let bad = match wc {
            WhereClause::Eq(l, r)
            | WhereClause::Ne(l, r)
            | WhereClause::Lt(l, r)
            | WhereClause::Gt(l, r)
            | WhereClause::Le(l, r)
            | WhereClause::Ge(l, r) => expr_contains_aggregate(l) || expr_contains_aggregate(r),
            WhereClause::Expr(e) => expr_contains_aggregate(e),
        };
        if bad {
            return Err(
                "SyntaxError(InvalidAggregation): aggregation is not allowed in WHERE".to_string(),
            );
        }
        Ok(())
    };
    match stmt {
        Statement::Query(q) => {
            if let Some(wc) = &q.where_clause {
                check_where(wc)?;
            }
            for part in &q.parts {
                match part {
                    QueryPart::Match { where_clause, .. }
                    | QueryPart::OptionalMatch { where_clause, .. }
                    | QueryPart::With { where_clause, .. } => {
                        if let Some(wc) = where_clause {
                            check_where(wc)?;
                        }
                    }
                    _ => {}
                }
            }
        }
        Statement::Union(u) => {
            check_no_aggregate_in_where(&u.left)?;
            check_no_aggregate_in_where(&u.right)?;
        }
        Statement::Pipeline(stmts) => {
            for s in stmts {
                check_no_aggregate_in_where(s)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Collect every projection (RETURN or WITH) item list reachable from a statement.
fn projection_lists(stmt: &Statement) -> Vec<&[ReturnItem]> {
    let mut out: Vec<&[ReturnItem]> = Vec::new();
    match stmt {
        Statement::Query(q) => {
            out.push(&q.return_clause.items);
            for part in &q.parts {
                if let QueryPart::With { items, .. } = part {
                    out.push(items);
                }
            }
        }
        Statement::CreateAndReturn(c) => out.push(&c.return_clause.items),
        Statement::SetAndReturn(s) => out.push(&s.return_clause.items),
        Statement::DeleteAndReturn(d) => out.push(&d.return_clause.items),
        Statement::RemoveAndReturn(r) => out.push(&r.return_clause.items),
        Statement::MergeAndReturn(m) => out.push(&m.return_clause.items),
        Statement::Union(u) => {
            out.extend(projection_lists(&u.left));
            out.extend(projection_lists(&u.right));
        }
        Statement::Pipeline(stmts) => {
            for s in stmts {
                out.extend(projection_lists(s));
            }
        }
        _ => {}
    }
    out
}

/// Two projected columns may not share a name. A column's name is its explicit
/// alias, or the bare variable name when the item is a plain variable reference.
fn check_duplicate_columns(items: &[ReturnItem]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for item in items {
        let name = match (&item.alias, &item.expr) {
            (Some(a), _) => Some(a.clone()),
            (None, Expr::Prop(v, p)) if p.is_empty() => Some(v.clone()),
            _ => None,
        };
        if let Some(name) = name {
            if !seen.insert(name.clone()) {
                return Err(format!(
                    "SyntaxError(ColumnNameConflict): multiple result columns with the name '{}'",
                    name
                ));
            }
        }
    }
    Ok(())
}

/// The argument of an aggregation must not itself contain an aggregation
/// (`count(count(*))`) or a non-deterministic function (`count(rand())`).
fn check_aggregation_arg(expr: &Expr) -> Result<(), String> {
    match expr {
        Expr::Agg(_, inner) => {
            if expr_contains_aggregate(inner) {
                return Err(
                    "SyntaxError(NestedAggregation): an aggregate may not contain another aggregate"
                        .to_string(),
                );
            }
            if expr_contains_rand(inner) {
                return Err(
                    "SyntaxError(NonConstantExpression): an aggregate may not contain a non-deterministic function"
                        .to_string(),
                );
            }
            check_aggregation_arg(inner)
        }
        Expr::BinaryOp { left, right, .. } => {
            check_aggregation_arg(left)?;
            check_aggregation_arg(right)
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                check_aggregation_arg(a)?;
            }
            Ok(())
        }
        Expr::IsNull(e) | Expr::IsNotNull(e) | Expr::Not(e) => check_aggregation_arg(e),
        Expr::Subscript { expr, index } => {
            check_aggregation_arg(expr)?;
            check_aggregation_arg(index)
        }
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            if let Some(s) = subject {
                check_aggregation_arg(s)?;
            }
            for arm in arms {
                check_aggregation_arg(&arm.when)?;
                check_aggregation_arg(&arm.then)?;
            }
            if let Some(e) = else_expr {
                check_aggregation_arg(e)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// True if `expr` contains an aggregation function or `count(*)` anywhere.
fn expr_contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Agg(..) | Expr::CountStar => true,
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_aggregate(left) || expr_contains_aggregate(right)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_contains_aggregate),
        Expr::IsNull(e) | Expr::IsNotNull(e) | Expr::Not(e) => expr_contains_aggregate(e),
        Expr::Subscript { expr, index } => {
            expr_contains_aggregate(expr) || expr_contains_aggregate(index)
        }
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            subject.as_ref().is_some_and(|s| expr_contains_aggregate(s))
                || arms
                    .iter()
                    .any(|a| expr_contains_aggregate(&a.when) || expr_contains_aggregate(&a.then))
                || else_expr
                    .as_ref()
                    .is_some_and(|e| expr_contains_aggregate(e))
        }
        _ => false,
    }
}

/// True if `expr` calls `rand()` anywhere.
fn expr_contains_rand(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall { name, args } => {
            name.eq_ignore_ascii_case("rand") || args.iter().any(expr_contains_rand)
        }
        Expr::Agg(_, inner) => expr_contains_rand(inner),
        Expr::BinaryOp { left, right, .. } => expr_contains_rand(left) || expr_contains_rand(right),
        Expr::IsNull(e) | Expr::IsNotNull(e) | Expr::Not(e) => expr_contains_rand(e),
        Expr::Subscript { expr, index } => expr_contains_rand(expr) || expr_contains_rand(index),
        Expr::Case {
            subject,
            arms,
            else_expr,
        } => {
            subject.as_ref().is_some_and(|s| expr_contains_rand(s))
                || arms
                    .iter()
                    .any(|a| expr_contains_rand(&a.when) || expr_contains_rand(&a.then))
                || else_expr.as_ref().is_some_and(|e| expr_contains_rand(e))
        }
        _ => false,
    }
}

// ─── Value-vs-element variable conflicts ──────────────────────────────────────

/// Detect a variable that is bound to a value (a literal, a property access, an
/// arithmetic result, and the like) and then used as a node, relationship, or
/// path variable in a pattern. openCypher raises VariableTypeConflict for this.
fn validate_value_var_conflicts(stmt: &Statement) -> Result<(), String> {
    if let Statement::Query(q) = stmt {
        let mut element_vars = std::collections::HashSet::new();
        let mut value_vars = std::collections::HashSet::new();
        for mc in &q.match_clauses {
            check_pattern_value_conflict(&mc.pattern, &value_vars)?;
            collect_pattern_bound_vars(&mc.pattern, &mut element_vars);
        }
        for part in &q.parts {
            match part {
                QueryPart::Match { match_clauses, .. }
                | QueryPart::OptionalMatch { match_clauses, .. } => {
                    for mc in match_clauses {
                        check_pattern_value_conflict(&mc.pattern, &value_vars)?;
                        collect_pattern_bound_vars(&mc.pattern, &mut element_vars);
                    }
                }
                QueryPart::Create { patterns } => {
                    for p in patterns {
                        check_pattern_value_conflict(p, &value_vars)?;
                        collect_pattern_bound_vars(p, &mut element_vars);
                    }
                }
                QueryPart::With { items, .. } => {
                    for item in items {
                        if let Some(ref alias) = item.alias {
                            if is_value_expr(&item.expr, &element_vars) {
                                value_vars.insert(alias.clone());
                            } else {
                                element_vars.insert(alias.clone());
                            }
                        }
                    }
                }
                QueryPart::Unwind { variable, .. } => {
                    value_vars.insert(variable.clone());
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// True if `expr` denotes a value rather than a graph element. The only graph-element
/// expression is a bare reference to a variable already bound as a node or relationship.
fn is_value_expr(expr: &Expr, element_vars: &std::collections::HashSet<String>) -> bool {
    match expr {
        // Bare reference to a graph-element variable: passes the element through.
        Expr::Prop(v, p) if p.is_empty() => !element_vars.contains(v),
        // A null binding is type-compatible with any graph element, so reusing it as
        // a node, relationship, or path variable is allowed (it matches nothing).
        Expr::Literal(Literal::Null) => false,
        // Functions that select among their arguments can return a node,
        // relationship, or path (e.g. `coalesce(b, c)`, `head(list)`), so their
        // result stays type-compatible with a graph-element variable.
        Expr::FunctionCall { name, .. }
            if matches!(name.to_lowercase().as_str(), "coalesce" | "head" | "last") =>
        {
            false
        }
        _ => true,
    }
}

/// Reject a pattern that uses any variable in `value_vars` as a node, relationship,
/// or path variable.
fn check_pattern_value_conflict(
    pattern: &Pattern,
    value_vars: &std::collections::HashSet<String>,
) -> Result<(), String> {
    let conflict = |v: &Option<String>| -> Result<(), String> {
        if let Some(v) = v {
            if value_vars.contains(v) {
                return Err(format!(
                    "SyntaxError(VariableTypeConflict): variable '{}' is bound to a value and cannot be used as a graph element",
                    v
                ));
            }
        }
        Ok(())
    };
    conflict(&pattern.path_variable)?;
    conflict(&pattern.node.variable)?;
    for (rel, target) in &pattern.rels {
        // A variable-length relationship variable binds to a list, so reusing a
        // list value there (e.g. `WITH [r1, r2] AS rs MATCH ()-[rs*]->()`) is a
        // legal openCypher construct, not a type conflict.
        if rel.range.is_none() {
            conflict(&rel.variable)?;
        }
        conflict(&target.variable)?;
    }
    Ok(())
}

// ─── Recursion-depth guard ──────────────────────────────────────────────────

// The parser is a recursive-descent combinator parser (built with `chumsky`),
// and several constructs recurse with the nesting depth of the source: bracketed
// sub-expressions `( [ {`, `CASE`/`END`, `UNION` chains, `FOREACH` bodies, and
// chained binary or unary operators. Every later pass over the resulting AST
// recurses the same way: validation, planning, optimization, execution
// (`evaluate_expr`), and `Drop`. Without a guard a deeply nested query string
// overflows the stack and aborts the whole process with SIGABRT rather than
// returning an error.
//
// The guard rejects truly pathological input (thousands of levels) before any
// AST is built, which bounds the parser and every later pass at once. It is not
// the mechanism that keeps realistic deep input safe: that is handled by running
// both the deep parse and the deep execution on dedicated large-stack threads
// (see `PARSE_THREAD_STACK` here and the executor's large-stack dispatch). The
// weighted budget is therefore sized to the large stacks those threads provide,
// not to a small worker stack.
//
// The cost of one nesting level is weighted per construct: each open level adds
// its per-level stack cost, and the deepest weighted path must stay within the
// budget. A single combined budget is required because the constructs can nest
// inside one another and their frames add up on one call path. The scan is a
// single iterative pass over the tokens with no recursion of its own.
//
// A separate, smaller threshold (`SMALL_STACK_EXEC_BUDGET_KB`) decides whether a
// query's execution must move to a large-stack thread: shallow queries (the
// common case) execute inline on the caller stack, and only deeper ones pay the
// thread hop.

/// Per-level stack cost (KiB), each padded above the measured value.
const BRACKET_COST_KB: usize = 70;
const CASE_COST_KB: usize = 14;
const OP_COST_KB: usize = 21;
const UNION_COST_KB: usize = 14;
/// Maximum weighted nesting cost (KiB) accepted at all. Sized so realistic deep
/// input (for example a 40-deep nested literal) parses and executes on the
/// large-stack threads, while genuinely pathological input (thousands of levels)
/// that would overflow even those is still rejected up front.
const MAX_NESTING_COST_KB: usize = 12_000;

/// Weighted nesting cost (KiB) up to which a query executes inline on the
/// caller stack. Above it, execution moves to a large-stack thread, because
/// expression evaluation recurses with the nesting depth and a deeply nested
/// literal would otherwise overflow a small (for example 2 MiB) worker stack.
pub(crate) const SMALL_STACK_EXEC_BUDGET_KB: usize = 1000;

/// Parse stays on the caller stack only when bracket and `CASE` nesting (the
/// parser's costly frames) and the operator and `UNION` depth are all small
/// enough to be safe even on a 2 MiB stack.
const INLINE_BRACKET_CASE_DEPTH: usize = 2;
const INLINE_OP_DEPTH: usize = 48;
const INLINE_UNION_DEPTH: usize = 16;
/// Stack size for the dedicated parse thread used for deeper inputs. Large enough
/// for any input within `MAX_NESTING_COST_KB`; the address space is reserved
/// lazily, so only pages actually touched by a given parse cost memory.
const PARSE_THREAD_STACK: usize = 256 * 1024 * 1024;

/// A token is an operator (it makes the expression parser recurse on an operand)
/// when it is one of these keywords.
fn is_operator_keyword(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "AND" | "OR" | "XOR" | "NOT" | "IN" | "IS" | "CONTAINS" | "STARTS" | "ENDS"
    )
}

/// A keyword that begins a clause, and therefore a fresh expression context. The
/// operator chain accumulated in the previous clause ends here, so it no longer
/// contributes to the deepest call path. A genuine deep operator chain lives
/// inside a single expression and contains none of these, so resetting at a
/// clause boundary never hides one.
fn is_clause_keyword(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "MATCH"
            | "OPTIONAL"
            | "CREATE"
            | "MERGE"
            | "DELETE"
            | "DETACH"
            | "SET"
            | "REMOVE"
            | "RETURN"
            | "WITH"
            | "WHERE"
            | "UNWIND"
            | "CALL"
            | "YIELD"
            | "FOREACH"
            | "ORDER"
            | "SKIP"
            | "LIMIT"
    )
}

/// Worst-case nesting measured from the token stream.
#[derive(Default)]
struct Nesting {
    /// Deepest weighted downstream stack cost on a single call path (KiB).
    cost_kb: usize,
    /// Deepest simultaneous bracket plus `CASE` nesting.
    bracket_case: usize,
    /// Deepest operator-chain depth.
    op: usize,
    /// Deepest `UNION` nesting within a pipeline segment.
    union: usize,
}

/// One open nesting level on the scan's explicit stack.
struct Frame {
    /// Stack cost contributed by opening this level (bracket or `CASE`).
    base_kb: usize,
    /// Operators chained directly inside this level.
    op_run: usize,
}

/// Compute worst-case nesting from the token stream. The scan is iterative: it
/// maintains an explicit stack of open levels instead of recursing.
fn scan_nesting(tokens: &[(Tok, SimpleSpan)]) -> Nesting {
    // The first frame is the implicit top level (no opening bracket).
    let mut frames: Vec<Frame> = vec![Frame {
        base_kb: 0,
        op_run: 0,
    }];
    let mut cost_kb: usize = 0;
    let mut union_depth: usize = 0;
    let mut op_total: usize = 0;
    let mut out = Nesting::default();

    for (tok, _) in tokens {
        match tok {
            Tok::LParen | Tok::LBrack | Tok::LBrace => {
                frames.push(Frame {
                    base_kb: BRACKET_COST_KB,
                    op_run: 0,
                });
                cost_kb += BRACKET_COST_KB;
            }
            Tok::RParen | Tok::RBrack | Tok::RBrace => {
                close_frame(&mut frames, &mut cost_kb, &mut op_total)
            }
            Tok::Comma => {
                // A new list element or argument restarts the operator chain at
                // this level.
                reset_operator_run(&mut frames, &mut cost_kb, &mut op_total);
            }
            Tok::Semi => {
                // Pipeline statements parse iteratively, so each segment starts
                // from a clean stack.
                frames.truncate(1);
                frames[0].op_run = 0;
                cost_kb = 0;
                op_total = 0;
                union_depth = 0;
            }
            Tok::Eq
            | Tok::Ne
            | Tok::Lt
            | Tok::Gt
            | Tok::Le
            | Tok::Ge
            | Tok::RegexEq
            | Tok::Plus
            | Tok::Minus
            | Tok::Star
            | Tok::Slash
            | Tok::Percent
            | Tok::Caret => add_operator(&mut frames, &mut cost_kb, &mut op_total),
            Tok::Ident(name) => {
                if name.eq_ignore_ascii_case("CASE") {
                    frames.push(Frame {
                        base_kb: CASE_COST_KB,
                        op_run: 0,
                    });
                    cost_kb += CASE_COST_KB;
                } else if name.eq_ignore_ascii_case("END") {
                    close_frame(&mut frames, &mut cost_kb, &mut op_total);
                } else if name.eq_ignore_ascii_case("UNION") {
                    union_depth += 1;
                    cost_kb += UNION_COST_KB;
                } else if is_operator_keyword(name) {
                    add_operator(&mut frames, &mut cost_kb, &mut op_total);
                } else if is_clause_keyword(name) {
                    // A clause boundary ends the previous clause's operator chain.
                    // A relationship pattern's leading dash (`-[:T]->`) lexes as a
                    // subtraction operator, so without this reset a flat query with
                    // many sibling clauses (each holding pattern dashes) would
                    // accumulate operator cost across breadth and read as deep.
                    reset_operator_run(&mut frames, &mut cost_kb, &mut op_total);
                }
            }
            _ => {}
        }
        out.cost_kb = out.cost_kb.max(cost_kb);
        out.bracket_case = out.bracket_case.max(frames.len() - 1);
        out.op = out.op.max(op_total);
        out.union = out.union.max(union_depth);
    }

    out
}

/// Record one operator chained inside the innermost open level.
fn add_operator(frames: &mut [Frame], cost_kb: &mut usize, op_total: &mut usize) {
    if let Some(f) = frames.last_mut() {
        f.op_run += 1;
    }
    *cost_kb += OP_COST_KB;
    *op_total += 1;
}

/// End the operator chain accumulated directly inside the innermost open level,
/// deducting its stack-cost contribution. A comma (a new list element or
/// argument) and a clause keyword (a fresh expression context) both terminate
/// the current chain, so it no longer counts toward the deepest call path.
fn reset_operator_run(frames: &mut [Frame], cost_kb: &mut usize, op_total: &mut usize) {
    if let Some(top) = frames.last_mut() {
        *cost_kb -= top.op_run * OP_COST_KB;
        *op_total -= top.op_run;
        top.op_run = 0;
    }
}

/// Pop the innermost open level, deducting its contribution. The top-level frame
/// is never popped; an unbalanced closer is left for the real parser to report.
fn close_frame(frames: &mut Vec<Frame>, cost_kb: &mut usize, op_total: &mut usize) {
    if frames.len() > 1 {
        if let Some(f) = frames.pop() {
            *cost_kb -= f.base_kb + f.op_run * OP_COST_KB;
            *op_total -= f.op_run;
        }
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Parse a Cypher query string into a `Statement` AST.
pub fn parse(cypher: &str) -> Result<Statement, CypherError> {
    parse_with_exec_depth(cypher).map(|(stmt, _)| stmt)
}

/// Parse a Cypher query string, also reporting whether its expression nesting is
/// deep enough that execution must run on a large-stack thread. Evaluation
/// recurses with the nesting depth, so a deeply nested literal would overflow a
/// small worker stack; the executor uses this flag to move such a query off the
/// caller stack. The flag is `false` for the common shallow case.
pub(crate) fn parse_with_exec_depth(cypher: &str) -> Result<(Statement, bool), CypherError> {
    let tokens = lexer().parse(cypher).into_result().map_err(|errs| {
        let msg = errs
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        CypherError::Parse(msg)
    })?;

    let nesting = scan_nesting(&tokens);
    if nesting.cost_kb > MAX_NESTING_COST_KB {
        return Err(CypherError::Parse(
            "SyntaxError: query is too deeply nested; reduce nesting of brackets, \
             CASE expressions, UNION clauses, or chained operators"
                .to_string(),
        ));
    }

    // Parsing and validation recurse with the source nesting depth. Run them on a
    // large-stack thread once the input is deep enough that the caller stack may
    // be too small.
    let run = || -> Result<Statement, CypherError> {
        let eoi = SimpleSpan::from(cypher.len()..cypher.len());
        let stream = tokens.as_slice().split_token_span(eoi);

        let statement = pipeline_parser(cypher)
            .parse(stream)
            .into_result()
            .map_err(|errs| {
                let msg = errs
                    .into_iter()
                    .map(|e| format!("{:?}", e))
                    .collect::<Vec<_>>()
                    .join("; ");
                CypherError::Parse(msg)
            })?;

        validate_statement(&statement).map_err(CypherError::Parse)?;
        Ok(statement)
    };

    let inline = nesting.bracket_case <= INLINE_BRACKET_CASE_DEPTH
        && nesting.op <= INLINE_OP_DEPTH
        && nesting.union <= INLINE_UNION_DEPTH;
    let stmt = if inline {
        run()?
    } else {
        std::thread::scope(|scope| {
            let handle = std::thread::Builder::new()
                .stack_size(PARSE_THREAD_STACK)
                .spawn_scoped(scope, run)
                .map_err(|e| CypherError::Parse(format!("failed to spawn parse thread: {e}")))?;
            handle
                .join()
                .map_err(|_| CypherError::Parse("parse thread panicked".to_string()))?
        })?
    };

    let exec_needs_large_stack = nesting.cost_kb > SMALL_STACK_EXEC_BUDGET_KB;
    Ok((stmt, exec_needs_large_stack))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // --- Numeric literal range and escape validation ---

    /// Out-of-range numeric literals are rejected at parse time, while the
    /// boundary values (`i64::MAX`, and `i64::MIN` written as a negated 2^63
    /// literal) are accepted. Malformed `\u` escapes are also rejected.
    #[test]
    fn numeric_literal_range_and_escape_validation() {
        // Boundary values that must parse.
        assert!(parse("RETURN 9223372036854775807 AS x").is_ok());
        assert!(parse("RETURN -9223372036854775808 AS x").is_ok());
        assert!(parse("RETURN 0x7FFFFFFFFFFFFFFF AS x").is_ok());
        assert!(parse("RETURN -0x8000000000000000 AS x").is_ok());
        assert!(parse("RETURN -0o1000000000000000000000 AS x").is_ok());

        // Standalone 2^63 (one past i64::MAX) is out of range.
        assert!(parse("RETURN 9223372036854775808 AS x").is_err());
        assert!(parse("RETURN 0x8000000000000000 AS x").is_err());
        assert!(parse("RETURN 0o1000000000000000000000 AS x").is_err());

        // Magnitudes beyond 2^63 are always out of range, signed either way.
        assert!(parse("RETURN -9223372036854775809 AS x").is_err());
        assert!(parse("RETURN -0x8000000000000001 AS x").is_err());
        assert!(parse("RETURN -0o1000000000000000000001 AS x").is_err());

        // Floating point overflow and malformed unicode escapes.
        assert!(parse("RETURN 1.34E999").is_err());
        assert!(parse("RETURN '\\uH'").is_err());
        assert!(parse("RETURN '\\u00e9'").is_ok());
    }

    /// A negated 2^63 literal folds to `i64::MIN`, not a `0 - x` subtraction.
    #[test]
    fn negated_min_integer_folds_to_literal() {
        let stmt = parse("RETURN -9223372036854775808 AS x").unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert_eq!(
            q.return_clause.items[0].expr,
            Expr::Literal(Literal::Int(i64::MIN))
        );
    }

    // --- Multi-label patterns and SET/REMOVE labels ---

    /// A node pattern retains every `:Label` segment in source order.
    #[test]
    fn parse_multi_label_node_pattern() {
        let stmt = parse("MATCH (n:A:B:C) RETURN n").unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert_eq!(q.match_clauses[0].pattern.node.labels, vec!["A", "B", "C"]);
    }

    /// `SET n:Label` parses to a `SetItem::Labels`, and `SET n.p = v` to a
    /// `SetItem::Property`.
    #[test]
    fn parse_set_label_and_property() {
        let stmt = parse("MATCH (n) SET n:Foo:Bar").unwrap();
        let set_items = match stmt {
            Statement::Set(s) => s.set_items,
            Statement::Query(q) => match q.parts.into_iter().find_map(|p| match p {
                QueryPart::Set { items } => Some(items),
                _ => None,
            }) {
                Some(items) => items,
                None => panic!("no SET part"),
            },
            other => panic!("unexpected statement: {other:?}"),
        };
        assert_eq!(set_items.len(), 1);
        match &set_items[0] {
            SetItem::Labels { variable, labels } => {
                assert_eq!(variable, "n");
                assert_eq!(labels, &vec!["Foo".to_string(), "Bar".to_string()]);
            }
            other => panic!("expected label set item, got {other:?}"),
        }
    }

    /// DELETE accepts arbitrary expressions (subscripts, property access), not
    /// just bare variables, so list/map/path deletion can be planned.
    #[test]
    fn parse_delete_expression_targets() {
        let stmt = parse("MATCH (a) WITH collect(a) AS xs DELETE xs[0], xs[1]").unwrap();
        let targets = match stmt {
            Statement::Delete(d) => d.targets,
            Statement::Query(q) => q
                .parts
                .into_iter()
                .find_map(|p| match p {
                    QueryPart::Delete { targets, .. } => Some(targets),
                    _ => None,
                })
                .expect("no DELETE part"),
            other => panic!("unexpected statement: {other:?}"),
        };
        assert_eq!(targets.len(), 2);
        // Each target is a subscript expression, not a bare identifier.
        assert!(matches!(&targets[0], Expr::Subscript { .. }));
    }

    /// `REMOVE n:Label` parses to a `RemoveItem::Label`.
    #[test]
    fn parse_remove_label() {
        let stmt = parse("MATCH (n) REMOVE n:L1:L2").unwrap();
        let items = match stmt {
            Statement::Remove(r) => r.items,
            Statement::Query(q) => q
                .parts
                .into_iter()
                .find_map(|p| match p {
                    QueryPart::Remove { items } => Some(items),
                    _ => None,
                })
                .expect("no REMOVE part"),
            other => panic!("unexpected statement: {other:?}"),
        };
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], RemoveItem::Label { label, .. } if label == "L1"));
        assert!(matches!(&items[1], RemoveItem::Label { label, .. } if label == "L2"));
    }

    // --- Multi-clause chaining ---

    /// Statements that chain clauses beyond the fixed shapes of the dedicated
    /// statement parsers must parse as a single `Statement::Query` whose `parts`
    /// hold the full clause sequence, rather than being rejected after a prefix.
    #[test]
    fn parse_multi_clause_chaining() {
        let cases = [
            "CREATE (a) CREATE (b)",
            "MATCH (a) SET a.x = 1 CREATE (b)",
            "MERGE (a) MERGE (b)",
            "MATCH (a) SET a.x = 1 WITH a RETURN a",
            "MATCH (a) SET a.x = 1 SET a.y = 2",
            "MATCH (a) DELETE a CREATE (b)",
            "CREATE (a) WITH a MATCH (b) RETURN b",
            "MATCH (a) REMOVE a.x SET a.y = 1",
            "MERGE (a) ON CREATE SET a.x = 1 MERGE (b)",
        ];
        for q in cases {
            let stmt = parse(q).unwrap_or_else(|e| panic!("failed to parse {q:?}: {e}"));
            assert!(
                matches!(stmt, Statement::Query(ref query) if query.parts.len() >= 2),
                "expected {q:?} to parse as a multi-part Statement::Query, got {stmt:?}"
            );
        }
    }

    /// The dedicated single-statement variants (with their write-lock semantics)
    /// must be preserved when no further clauses follow.
    #[test]
    fn parse_single_statement_variants_preserved() {
        assert!(matches!(parse("CREATE (a)").unwrap(), Statement::Create(_)));
        assert!(matches!(
            parse("CREATE (a) RETURN a").unwrap(),
            Statement::CreateAndReturn(_)
        ));
        assert!(matches!(
            parse("MATCH (a) SET a.x = 1").unwrap(),
            Statement::Set(_)
        ));
        assert!(matches!(
            parse("MATCH (a) SET a.x = 1 RETURN a").unwrap(),
            Statement::SetAndReturn(_)
        ));
        assert!(matches!(
            parse("MATCH (a) DELETE a").unwrap(),
            Statement::Delete(_)
        ));
        assert!(matches!(parse("MERGE (a)").unwrap(), Statement::Merge(_)));
        assert!(matches!(
            parse("MERGE (a) RETURN a").unwrap(),
            Statement::MergeAndReturn(_)
        ));
        assert!(matches!(
            parse("MATCH (a) RETURN a UNION MATCH (b) RETURN b").unwrap(),
            Statement::Union(_)
        ));
        assert!(matches!(
            parse("CREATE (a); CREATE (b)").unwrap(),
            Statement::Pipeline(_)
        ));
    }

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
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains('r'), "error should mention variable name");
    }

    #[test]
    fn rel_var_used_as_node_errors() {
        // ()-[r]-(r): 'r' as both relationship and node

        let result = parse("MATCH ()-[r]-(r) RETURN r");
        assert!(
            result.is_err(),
            "expected error when relationship variable 'r' is also used as node"
        );
    }

    #[test]
    fn cross_match_node_then_rel_var_errors() {
        // MATCH (r) MATCH ()-[r]-(): 'r' as node then relationship

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

    // --- Semantic validation: CREATE structure, aggregation, and value-type conflicts ---

    fn err_kind(q: &str) -> String {
        match parse(q) {
            Ok(_) => "OK".to_string(),
            Err(e) => {
                let msg = match e {
                    CypherError::Parse(msg) => msg,
                    other => other.to_string(),
                };
                msg.split([':', ')'])
                    .next()
                    .unwrap_or("?")
                    .trim_end_matches('(')
                    .to_string()
            }
        }
    }

    #[test]
    fn create_relationship_without_type_is_rejected() {
        assert!(parse("CREATE ()-->()").is_err());
    }

    #[test]
    fn create_relationship_with_multiple_types_is_rejected() {
        assert!(parse("CREATE ()-[:A|:B]->()").is_err());
    }

    #[test]
    fn create_undirected_relationship_is_rejected() {
        assert!(parse("CREATE (a)-[:FOO]-(b)").is_err());
    }

    #[test]
    fn create_variable_length_relationship_is_rejected() {
        assert!(parse("CREATE ()-[:FOO*2]->()").is_err());
    }

    #[test]
    fn create_already_bound_node_is_rejected() {
        assert!(parse("MATCH (a) CREATE (a)").is_err());
        assert!(parse("MATCH (a) CREATE (a {name: 'foo'}) RETURN a").is_err());
        assert!(parse("CREATE (n:Foo) CREATE (n:Bar)-[:OWNS]->(:Dog)").is_err());
    }

    #[test]
    fn create_already_bound_relationship_is_rejected() {
        assert!(parse("MATCH ()-[r]->() CREATE ()-[r]->()").is_err());
    }

    #[test]
    fn bound_node_as_bare_endpoint_is_allowed() {
        // Reusing a bound node only to attach a new relationship is valid.
        assert!(parse("MATCH (a) CREATE (a)-[:R]->(b) RETURN b").is_ok());
        assert!(parse("MATCH (n) WITH n MATCH (n)-->(m) RETURN m").is_ok());
    }

    #[test]
    fn duplicate_return_column_is_rejected() {
        assert!(parse("RETURN 1 AS a, 2 AS a").is_err());
        assert!(parse("RETURN 1 AS a, 2 AS b").is_ok());
    }

    #[test]
    fn nested_aggregation_is_rejected() {
        assert!(parse("RETURN count(count(*))").is_err());
        assert_eq!(
            err_kind("RETURN count(count(*))"),
            "SyntaxError(NestedAggregation"
        );
    }

    #[test]
    fn rand_inside_aggregation_is_rejected() {
        assert!(parse("RETURN count(rand())").is_err());
    }

    #[test]
    fn value_bound_variable_used_as_pattern_element_is_rejected() {
        assert!(parse("WITH 123 AS r MATCH ()-[r]-() RETURN r").is_err());
        assert!(parse("WITH true AS n MATCH (n) RETURN n").is_err());
        assert!(parse("WITH 123 AS p MATCH p = ()-[]-() RETURN p").is_err());
    }

    #[test]
    fn passthrough_variable_reuse_is_allowed() {
        // A bare passthrough of a node variable through WITH is not a value binding.
        assert!(parse("MATCH (n) WITH n MATCH (n)-->(m) RETURN m").is_ok());
        assert!(parse("WITH 1 AS x RETURN x").is_ok());
    }

    #[test]
    fn null_bound_variable_used_as_pattern_element_is_allowed() {
        // null is type-compatible with any graph element; reusing it matches nothing.
        assert!(parse("WITH null AS a OPTIONAL MATCH p = (a)-[r]->() RETURN nodes(p)").is_ok());
        assert!(parse("WITH null AS a MATCH (a) RETURN a").is_ok());
    }

    #[test]
    fn parse_copy_statement() {
        let stmt = parse("COPY Person FROM 'person.csv' WITH {header: true, delimiter: ','}");
        assert!(stmt.is_ok());
        if let Ok(Statement::Copy(copy)) = stmt {
            assert_eq!(copy.target, "Person");
            assert_eq!(copy.filepath, "person.csv");
            assert!(copy.options.is_some());
        } else {
            panic!("Expected Statement::Copy");
        }

        let stmt_no_with = parse("COPY Person FROM 'person.csv'");
        assert!(stmt_no_with.is_ok());
        if let Ok(Statement::Copy(copy)) = stmt_no_with {
            assert_eq!(copy.target, "Person");
            assert_eq!(copy.filepath, "person.csv");
            assert!(copy.options.is_none());
        } else {
            panic!("Expected Statement::Copy");
        }
    }

    #[test]
    fn parse_import_export_statements() {
        let stmt = parse("EXPORT DATABASE 'backups/db1' WITH {format: 'jsonl'}");
        assert!(stmt.is_ok());
        if let Ok(Statement::ExportDatabase(export)) = stmt {
            assert_eq!(export.filepath, "backups/db1");
            assert!(export.options.is_some());
        } else {
            panic!("Expected Statement::ExportDatabase");
        }

        let stmt_import = parse("IMPORT DATABASE 'backups/db1'");
        assert!(stmt_import.is_ok());
        if let Ok(Statement::ImportDatabase(import)) = stmt_import {
            assert_eq!(import.filepath, "backups/db1");
        } else {
            panic!("Expected Statement::ImportDatabase");
        }
    }

    // --- Recursion-depth guard ---

    /// A query string with thousands of nested levels must return a parse error,
    /// not overflow the stack and abort the process. Each variant exercises a
    /// distinct recursion path in the parser (`UNION`, brackets, `CASE`,
    /// chained operators, map literals, and `FOREACH` bodies).
    #[test]
    fn deeply_nested_queries_are_rejected_not_aborted() {
        let union = std::iter::repeat("RETURN 1 AS x")
            .take(5000)
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        assert!(parse(&union).is_err());

        let mut list = String::from("1");
        for _ in 0..5000 {
            list = format!("[{}]", list);
        }
        assert!(parse(&format!("RETURN {list} AS x")).is_err());

        let mut case = String::from("1");
        for _ in 0..5000 {
            case = format!("CASE WHEN true THEN {case} ELSE 0 END");
        }
        assert!(parse(&format!("RETURN {case} AS x")).is_err());

        let and = std::iter::repeat("m.x = 1")
            .take(5000)
            .collect::<Vec<_>>()
            .join(" AND ");
        assert!(parse(&format!("MATCH (m) WHERE {and} RETURN m")).is_err());

        let mut map = String::from("1");
        for _ in 0..5000 {
            map = format!("{{a: {map}}}");
        }
        assert!(parse(&format!("RETURN {map} AS x")).is_err());

        let mut foreach = String::from("FOREACH (x IN [1] | CREATE (n))");
        for _ in 0..5000 {
            foreach = format!("FOREACH (y IN [1] | {foreach})");
        }
        assert!(parse(&foreach).is_err());
    }

    /// Ordinary queries, including ones with shallow nesting and several `AND`ed
    /// predicates, parse unaffected by the guard.
    #[test]
    fn ordinary_queries_parse_under_the_guard() {
        assert!(parse("MATCH (n:User) WHERE n.Id = 9028 RETURN n").is_ok());
        assert!(parse("UNWIND [1, 2, 3] AS x MATCH (n:User) WHERE n.Id = x RETURN n").is_ok());
        assert!(
            parse(
                "MATCH (a)-[r:KNOWS]->(b) \
                 WHERE a.age > 21 AND b.city = 'X' AND a.x = 1 AND a.y = 2 \
                 RETURN a.name, b.name"
            )
            .is_ok()
        );
        assert!(parse("RETURN {a: {b: {c: 1}}} AS m").is_ok());
        assert!(parse("RETURN CASE WHEN true THEN 1 ELSE 2 END AS x").is_ok());
        assert!(parse("RETURN 1 AS x UNION ALL RETURN 2 AS x UNION ALL RETURN 3 AS x").is_ok());
    }

    /// `scan_nesting` charges each construct and unwinds it on close, so its cost
    /// reflects the deepest single path, not the total token count.
    #[test]
    fn scan_nesting_tracks_depth_not_breadth() {
        let lex = |q: &str| lexer().parse(q).into_result().unwrap();

        // A flat, wide query stays cheap: each bracket pair opens and closes.
        let wide = scan_nesting(&lex("RETURN [1, 2, 3, 4, 5], [6, 7], [8, 9, 10] AS x"));
        assert_eq!(wide.bracket_case, 1);

        // Three nested brackets reach depth three.
        let deep = scan_nesting(&lex("RETURN [[[1]]] AS x"));
        assert_eq!(deep.bracket_case, 3);

        // Commas reset the operator chain, so sibling sums do not accumulate.
        let ops = scan_nesting(&lex("RETURN 1 + 2 + 3, 4 + 5 AS x"));
        assert_eq!(ops.op, 2);

        // A `UNION` chain is counted per segment.
        let unions = scan_nesting(&lex(
            "RETURN 1 AS x UNION RETURN 2 AS x UNION RETURN 3 AS x",
        ));
        assert_eq!(unions.union, 2);
    }

    /// A flat query with many sibling clauses must not read as deeply nested. A
    /// relationship pattern's leading dash (`-[:T]->`) lexes as a subtraction
    /// operator, so without a clause-boundary reset the operator cost summed
    /// across every clause. Regression for the openCypher TCK
    /// `Create4 [2] Many CREATE clauses` scenario, which the guard wrongly
    /// rejected as too deeply nested.
    #[test]
    fn many_flat_clauses_with_pattern_dashes_parse() {
        let lex = |q: &str| lexer().parse(q).into_result().unwrap();

        let mut q = String::from("CREATE (hf:School {name: 'X'})\n");
        for i in 0..80 {
            q.push_str(&format!(
                "CREATE (hf)-[:STAFF]->(t{i}:Teacher {{name: 'N{i}'}})\n"
            ));
        }
        // The pattern dashes do not accumulate across clauses.
        let nesting = scan_nesting(&lex(&q));
        assert!(
            nesting.op <= 1,
            "operator depth {} must not grow with the clause count",
            nesting.op
        );
        assert!(parse(&q).is_ok(), "a flat many-clause query must parse");
    }

    /// The clause-boundary reset ends only the previous clause's operator chain.
    /// A genuine deep chain within one expression contains no clause keyword, so
    /// it still registers its full depth, and one long enough to exceed the
    /// budget is still rejected.
    #[test]
    fn clause_reset_does_not_hide_a_deep_single_chain() {
        let lex = |q: &str| lexer().parse(q).into_result().unwrap();

        // A chain whose weighted operator cost exceeds the maximum budget.
        let n = MAX_NESTING_COST_KB / OP_COST_KB + 50;
        let chain = std::iter::repeat("1")
            .take(n + 1)
            .collect::<Vec<_>>()
            .join(" - ");
        let nesting = scan_nesting(&lex(&format!("RETURN {chain} AS x")));
        assert!(
            nesting.op >= n,
            "a long single-clause operator chain must still register depth, got {}",
            nesting.op
        );
        assert!(parse(&format!("RETURN {chain} AS x")).is_err());
    }
}
