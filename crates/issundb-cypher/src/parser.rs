use std::collections::HashMap;

use crate::ast::*;

/// Parse a Cypher query string into a `Statement` AST.
pub fn parse(cypher: &str) -> Result<Statement, String> {
    let normalized = cypher.trim();
    let upper = normalized.to_ascii_uppercase();

    if upper.starts_with("CREATE") {
        let pattern_str = normalized["CREATE".len()..].trim();
        let pattern = parse_pattern(pattern_str)?;
        Ok(Statement::Create(CreateStatement { pattern }))
    } else if upper.starts_with("MATCH") {
        if upper.contains(" SET ") {
            parse_set_statement(normalized)
        } else if upper.contains(" DELETE ") {
            parse_delete_statement(normalized)
        } else {
            let query = parse_read_query(normalized)?;
            Ok(Statement::Query(query))
        }
    } else if upper.starts_with("UNWIND") || upper.starts_with("WITH") {
        // Queries that begin with UNWIND or WITH (followed by a RETURN) are read
        // queries without a leading MATCH clause.
        let query = parse_read_query(normalized)?;
        Ok(Statement::Query(query))
    } else {
        Err(format!("unsupported statement type in query: {}", cypher))
    }
}

fn parse_read_query(cypher: &str) -> Result<Query, String> {
    let boundaries = find_clause_boundaries(cypher)?;
    if boundaries.is_empty() {
        return Err("missing query clauses".into());
    }

    // The query must end with a RETURN clause.
    let (last_kw, last_idx) = &boundaries[boundaries.len() - 1];
    if last_kw != "RETURN" {
        return Err("Cypher read query must end with a RETURN clause".into());
    }

    let mut parts = Vec::new();

    // Iterate through all intermediate clauses before RETURN
    for i in 0..boundaries.len() - 1 {
        let (kw, start) = &boundaries[i];
        let end = boundaries[i + 1].1;
        let content = cypher[*start + kw.len()..end].trim();

        match kw.as_str() {
            "MATCH" => {
                let (match_str, where_str) = split_optional_where(content);
                let match_clauses = parse_match_clauses(match_str)?;
                let where_clause = where_str.map(parse_where_clause).transpose()?;
                parts.push(QueryPart::Match {
                    match_clauses,
                    where_clause,
                });
            }
            "WITH" => {
                let (with_str, where_str) = split_optional_where(content);
                let items = parse_return_items(with_str)?;
                let where_clause = where_str.map(parse_where_clause).transpose()?;
                parts.push(QueryPart::With {
                    items,
                    where_clause,
                });
            }
            "UNWIND" => {
                let as_idx = find_keyword_root_level(content, "AS")?.ok_or_else(|| {
                    format!("UNWIND clause missing AS keyword: UNWIND {}", content)
                })?;
                let expr_str = content[..as_idx].trim();
                let var_str = content[as_idx + "AS".len()..].trim();

                let expr = parse_expr(expr_str)?;
                let variable = var_str.to_string();
                parts.push(QueryPart::Unwind { expr, variable });
            }
            other => return Err(format!("unexpected intermediate query clause: {}", other)),
        }
    }

    // Parse the final RETURN clause
    let return_content = cypher[*last_idx + "RETURN".len()..].trim();
    let return_clause = parse_return_clause(return_content)?;

    // For backwards compatibility:
    let mut match_clauses = Vec::new();
    let mut where_clause = None;

    if !parts.is_empty() {
        if let QueryPart::Match {
            match_clauses: m,
            where_clause: w,
        } = &parts[0]
        {
            match_clauses = m.clone();
            where_clause = w.clone();
        }
    }

    Ok(Query {
        match_clauses,
        where_clause,
        return_clause,
        parts,
    })
}

fn split_optional_where(content: &str) -> (&str, Option<&str>) {
    if let Ok(Some(idx)) = find_keyword_root_level(content, "WHERE") {
        let before = &content[..idx].trim();
        let after = &content[idx + "WHERE".len()..].trim();
        (*before, Some(*after))
    } else {
        (content, None)
    }
}

fn find_keyword_root_level(text: &str, kw: &str) -> Result<Option<usize>, String> {
    // Returns a **byte offset** into `text` so callers can use it directly for
    // `&str` slicing.  Previously returned a char index which caused panics on
    // input containing non-ASCII characters before the keyword.
    //
    // All keywords are pure ASCII, so `kw.len()` equals both byte length and
    // char length; `ci + kw.len()` as a char index is therefore valid.
    let kw_byte_len = kw.len();

    let char_positions: Vec<(usize, char)> = text.char_indices().collect();
    let n = char_positions.len();

    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut paren_depth: usize = 0;
    let mut bracket_depth: usize = 0;
    let mut brace_depth: usize = 0;

    let mut ci = 0usize;
    while ci < n {
        let (byte_pos, c) = char_positions[ci];

        if c == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            ci += 1;
            continue;
        }
        if c == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            ci += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote {
            match c {
                '(' => paren_depth += 1,
                ')' => paren_depth = paren_depth.saturating_sub(1),
                '[' => bracket_depth += 1,
                ']' => bracket_depth = bracket_depth.saturating_sub(1),
                '{' => brace_depth += 1,
                '}' => brace_depth = brace_depth.saturating_sub(1),
                _ => {}
            }

            if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
                // Use `str::get` rather than direct indexing: it returns `None`
                // when the range crosses a multi-byte char boundary, which is
                // safe even if the keyword byte length overshoots the text end.
                if let Some(candidate) = text.get(byte_pos..byte_pos + kw_byte_len) {
                    if candidate.eq_ignore_ascii_case(kw) {
                        let is_start = ci == 0 || char_positions[ci - 1].1.is_ascii_whitespace();
                        // kw is pure ASCII so its char count == its byte count;
                        // the char after the keyword is at char index ci + kw_byte_len.
                        let after_ci = ci + kw_byte_len;
                        let is_end = after_ci >= n
                            || char_positions[after_ci].1.is_ascii_whitespace()
                            // Also treat `(` as a valid word boundary so that
                            // `WHERE(` is recognised (consistent with find_clause_boundaries).
                            || char_positions[after_ci].1 == '(';
                        if is_start && is_end {
                            return Ok(Some(byte_pos));
                        }
                    }
                }
            }
        }
        ci += 1;
    }
    Ok(None)
}

fn find_clause_boundaries(cypher: &str) -> Result<Vec<(String, usize)>, String> {
    // Returns a list of (keyword, byte_offset) pairs.  Previously returned char
    // indices, which caused panics when callers used them as byte offsets on
    // input that contains non-ASCII characters before a clause keyword.
    let mut clauses = Vec::new();
    let cypher_byte_len = cypher.len();

    let char_positions: Vec<(usize, char)> = cypher.char_indices().collect();
    let n = char_positions.len();

    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut paren_depth: usize = 0;
    let mut bracket_depth: usize = 0;
    let mut brace_depth: usize = 0;

    let mut ci = 0usize;
    while ci < n {
        let (byte_pos, c) = char_positions[ci];

        if c == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            ci += 1;
            continue;
        }
        if c == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            ci += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote {
            match c {
                '(' => paren_depth += 1,
                ')' => paren_depth = paren_depth.saturating_sub(1),
                '[' => bracket_depth += 1,
                ']' => bracket_depth = bracket_depth.saturating_sub(1),
                '{' => brace_depth += 1,
                '}' => brace_depth = brace_depth.saturating_sub(1),
                _ => {}
            }

            if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
                let mut matched_kw: Option<&str> = None;
                for kw in &["MATCH", "WITH", "UNWIND", "RETURN"] {
                    let kw_byte_len = kw.len(); // keywords are ASCII; byte len == char len
                    let end_byte = byte_pos + kw_byte_len;
                    if end_byte <= cypher_byte_len {
                        if let Some(candidate) = cypher.get(byte_pos..end_byte) {
                            if candidate.eq_ignore_ascii_case(kw) {
                                let is_start = ci == 0
                                    || char_positions[ci - 1].1.is_ascii_whitespace()
                                    || char_positions[ci - 1].1 == ';';
                                let after_ci = ci + kw_byte_len;
                                let is_end = after_ci >= n
                                    || char_positions[after_ci].1.is_ascii_whitespace()
                                    || char_positions[after_ci].1 == '('
                                    || char_positions[after_ci].1 == '{';
                                if is_start && is_end {
                                    matched_kw = Some(kw);
                                    break;
                                }
                            }
                        }
                    }
                }

                if let Some(kw) = matched_kw {
                    clauses.push((kw.to_string(), byte_pos));
                    // kw is pure ASCII so kw.len() == kw char count; advance ci
                    // past all keyword chars to avoid re-matching a prefix.
                    ci += kw.len();
                    continue;
                }
            }
        }

        ci += 1;
    }

    Ok(clauses)
}

fn parse_set_statement(cypher: &str) -> Result<Statement, String> {
    // Use find_keyword_root_level for all keyword searches so that label names
    // or variable names that contain the keyword as a substring (e.g., "RESET"
    // containing "SET") are not mistaken for the actual clause keyword.
    let match_idx = find_keyword_root_level(cypher, "MATCH")?.ok_or("missing MATCH clause")?;
    let set_idx = find_keyword_root_level(cypher, "SET")?.ok_or("missing SET clause")?;

    let where_idx_opt = find_keyword_root_level(cypher, "WHERE")?;

    let match_clause_str = if let Some(where_idx) = where_idx_opt {
        if where_idx > match_idx && where_idx < set_idx {
            &cypher[match_idx + "MATCH".len()..where_idx]
        } else {
            return Err("WHERE clause must be placed between MATCH and SET".into());
        }
    } else {
        &cypher[match_idx + "MATCH".len()..set_idx]
    };

    let match_clauses = parse_match_clauses(match_clause_str)?;

    let where_clause = if let Some(where_idx) = where_idx_opt {
        let where_clause_str = &cypher[where_idx + "WHERE".len()..set_idx];
        Some(parse_where_clause(where_clause_str)?)
    } else {
        None
    };

    let set_items_str = &cypher[set_idx + "SET".len()..];
    let mut set_items = Vec::new();
    for item_str in set_items_str.split(',') {
        let parts: Vec<&str> = item_str.split('=').map(|s| s.trim()).collect();
        if parts.len() != 2 {
            return Err("invalid SET assignment expression".into());
        }
        let prop_parts: Vec<&str> = parts[0].split('.').map(|s| s.trim()).collect();
        if prop_parts.len() != 2 {
            return Err("invalid SET property reference".into());
        }
        let variable = prop_parts[0].to_string();
        let property = prop_parts[1].to_string();
        let expr = parse_expr(parts[1])?;
        set_items.push(SetItem {
            variable,
            property,
            expr,
        });
    }

    Ok(Statement::Set(SetStatement {
        match_clauses,
        where_clause,
        set_items,
    }))
}

fn parse_delete_statement(cypher: &str) -> Result<Statement, String> {
    // Use find_keyword_root_level for all keyword searches so that label names
    // or variable names that contain the keyword as a substring are not
    // mistaken for the actual clause keyword.
    let match_idx = find_keyword_root_level(cypher, "MATCH")?.ok_or("missing MATCH clause")?;
    let delete_idx = find_keyword_root_level(cypher, "DELETE")?.ok_or("missing DELETE clause")?;

    let where_idx_opt = find_keyword_root_level(cypher, "WHERE")?;

    let match_clause_str = if let Some(where_idx) = where_idx_opt {
        if where_idx > match_idx && where_idx < delete_idx {
            &cypher[match_idx + "MATCH".len()..where_idx]
        } else {
            return Err("WHERE clause must be placed between MATCH and DELETE".into());
        }
    } else {
        &cypher[match_idx + "MATCH".len()..delete_idx]
    };

    let match_clauses = parse_match_clauses(match_clause_str)?;

    let where_clause = if let Some(where_idx) = where_idx_opt {
        let where_clause_str = &cypher[where_idx + "WHERE".len()..delete_idx];
        Some(parse_where_clause(where_clause_str)?)
    } else {
        None
    };

    let variables_str = &cypher[delete_idx + "DELETE".len()..];
    let variables = variables_str
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    Ok(Statement::Delete(DeleteStatement {
        match_clauses,
        where_clause,
        variables,
    }))
}

fn parse_match_clauses(s: &str) -> Result<Vec<MatchClause>, String> {
    let mut clauses = Vec::new();
    for part in s.split(',') {
        let pattern = parse_pattern(part.trim())?;
        clauses.push(MatchClause { pattern });
    }
    Ok(clauses)
}

fn parse_pattern(s: &str) -> Result<Pattern, String> {
    let mut remainder = s.trim();

    // Parse the seed node
    let (node, size) = parse_node_pattern(remainder)?;
    remainder = remainder[size..].trim();

    let mut rels = Vec::new();
    while !remainder.is_empty() {
        let (rel, rel_size) = parse_relationship_pattern(remainder)?;
        remainder = remainder[rel_size..].trim();

        let (target, target_size) = parse_node_pattern(remainder)?;
        remainder = remainder[target_size..].trim();

        rels.push((rel, target));
    }

    Ok(Pattern { node, rels })
}

fn parse_node_pattern(s: &str) -> Result<(NodePattern, usize), String> {
    if !s.starts_with('(') {
        return Err("node pattern must start with '('".into());
    }
    let end_idx = s.find(')').ok_or("missing closing node parenthesis ')'")?;
    let content = &s[1..end_idx].trim();

    // Parse properties map if present
    let prop_start = content.find('{');
    let (body, properties) = if let Some(idx) = prop_start {
        let prop_str = &content[idx..];
        let props = parse_properties_map(prop_str)?;
        (&content[..idx].trim(), Some(props))
    } else {
        (content, None)
    };

    let parts: Vec<&str> = body.split(':').collect();
    let variable = if parts[0].trim().is_empty() {
        None
    } else {
        Some(parts[0].trim().to_string())
    };

    let label = if parts.len() > 1 && !parts[1].trim().is_empty() {
        Some(parts[1].trim().to_string())
    } else {
        None
    };

    Ok((
        NodePattern {
            variable,
            label,
            properties,
        },
        end_idx + 1,
    ))
}

fn parse_relationship_pattern(s: &str) -> Result<(RelationshipPattern, usize), String> {
    let mut remainder = s.trim();
    let is_incoming = remainder.starts_with("<-");

    let left_dash = if is_incoming { "<-" } else { "-" };
    if !remainder.starts_with(left_dash) {
        return Err("invalid relationship syntax prefix".into());
    }
    remainder = &remainder[left_dash.len()..];

    if !remainder.starts_with('[') {
        return Err("relationship pattern must contain type bracket '['".into());
    }
    let bracket_end = remainder
        .find(']')
        .ok_or("missing relationship bracket ']'")?;
    let content = remainder[1..bracket_end].trim();
    remainder = &remainder[bracket_end + 1..];

    let is_outgoing = remainder.starts_with("->");
    let right_dash = if is_outgoing { "->" } else { "-" };
    if !remainder.starts_with(right_dash) {
        return Err("invalid relationship syntax suffix".into());
    }

    let (left_part, range_part) = if let Some(star_idx) = content.find('*') {
        (
            content[..star_idx].trim(),
            Some(content[star_idx + 1..].trim()),
        )
    } else {
        (content, None)
    };

    let parts: Vec<&str> = left_part.split(':').collect();
    let variable = if parts[0].trim().is_empty() {
        None
    } else {
        Some(parts[0].trim().to_string())
    };

    let rel_type = if parts.len() > 1 && !parts[1].trim().is_empty() {
        Some(parts[1].trim().to_string())
    } else {
        None
    };

    let range = if let Some(r_str) = range_part {
        let r_str = r_str.trim();
        if r_str.is_empty() {
            Some(RelRange {
                min: Some(1),
                max: None,
            })
        } else if r_str.contains("..") {
            let range_parts: Vec<&str> = r_str.split("..").collect();
            if range_parts.len() != 2 {
                return Err("invalid relationship range syntax".into());
            }
            let min = if range_parts[0].trim().is_empty() {
                Some(1)
            } else {
                Some(
                    range_parts[0]
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| "invalid min range")?,
                )
            };
            let max = if range_parts[1].trim().is_empty() {
                None
            } else {
                Some(
                    range_parts[1]
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| "invalid max range")?,
                )
            };
            Some(RelRange { min, max })
        } else {
            let hops = r_str
                .parse::<u32>()
                .map_err(|_| "invalid range exact hops value")?;
            Some(RelRange {
                min: Some(hops),
                max: Some(hops),
            })
        }
    } else {
        None
    };

    let consumed = left_dash.len() + bracket_end + 1 + right_dash.len();
    Ok((
        RelationshipPattern {
            variable,
            rel_type,
            is_incoming,
            range,
        },
        consumed,
    ))
}

fn parse_properties_map(s: &str) -> Result<HashMap<String, Literal>, String> {
    if !s.starts_with('{') || !s.ends_with('}') {
        return Err("properties map must be wrapped in curly braces".into());
    }
    let mut map = HashMap::new();
    let inner = &s[1..s.len() - 1].trim();
    if inner.is_empty() {
        return Ok(map);
    }
    for item in inner.split(',') {
        let parts: Vec<&str> = item.split(':').collect();
        if parts.len() != 2 {
            return Err("properties map contains invalid key-value pair".into());
        }
        let key = parts[0].trim().to_string();
        let val = parse_literal(parts[1].trim())?;
        map.insert(key, val);
    }
    Ok(map)
}

fn parse_where_clause(s: &str) -> Result<WhereClause, String> {
    let mut op_start = None;
    let mut op_len = 0;

    // Find comparison operator
    for &(op, len) in &[
        ("=", 1),
        ("!=", 2),
        ("<=", 2),
        (">=", 2),
        ("<", 1),
        (">", 1),
    ] {
        if let Some(idx) = s.find(op) {
            if op_start.is_none_or(|best| idx < best) {
                op_start = Some(idx);
                op_len = len;
            }
        }
    }

    let idx = op_start.ok_or("unsupported or missing comparison operator in WHERE clause")?;
    let left_str = &s[..idx].trim();
    let right_str = &s[idx + op_len..].trim();

    let left = parse_expr(left_str)?;
    let right = parse_expr(right_str)?;

    let op_str = &s[idx..idx + op_len];
    match op_str {
        "=" => Ok(WhereClause::Eq(left, right)),
        "!=" => Ok(WhereClause::Ne(left, right)),
        "<" => Ok(WhereClause::Lt(left, right)),
        ">" => Ok(WhereClause::Gt(left, right)),
        "<=" => Ok(WhereClause::Le(left, right)),
        ">=" => Ok(WhereClause::Ge(left, right)),
        _ => Err("unsupported operator".into()),
    }
}

fn parse_expr(s: &str) -> Result<Expr, String> {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix('$') {
        Ok(Expr::Param(rest.to_string()))
    } else if let Ok(lit) = parse_literal(trimmed) {
        // Attempt literal parsing before the `.contains('.')` branch so that
        // float literals like `3.14` are never mistaken for property references:
        // `3.14` contains a dot, would split into ["3","14"], and would be
        // returned as `Expr::Prop("3","14")` without this guard.
        Ok(Expr::Literal(lit))
    } else if trimmed.contains('.') {
        // Variable property reference (e.g. `n.name`).  Non-literal inputs that
        // contain a dot are property accesses.
        let parts: Vec<&str> = trimmed.split('.').collect();
        if parts.len() == 2 {
            Ok(Expr::Prop(
                parts[0].trim().to_string(),
                parts[1].trim().to_string(),
            ))
        } else {
            Err(format!("invalid expression: {}", trimmed))
        }
    } else if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Ok(Expr::Prop(trimmed.to_string(), "".to_string()))
    } else {
        Err(format!("invalid expression: {}", trimmed))
    }
}

fn parse_literal(s: &str) -> Result<Literal, String> {
    let trimmed = s.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let content = &trimmed[1..trimmed.len() - 1].trim();
        if content.is_empty() {
            return Ok(Literal::List(Vec::new()));
        }

        let mut list = Vec::new();
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut paren_depth = 0;
        let mut bracket_depth = 0;
        let mut brace_depth = 0;
        let mut last_start = 0;
        let chars: Vec<char> = content.chars().collect();
        let n = chars.len();

        let mut i = 0;
        while i < n {
            let c = chars[i];
            if c == '\'' && !in_double_quote {
                in_single_quote = !in_single_quote;
            } else if c == '"' && !in_single_quote {
                in_double_quote = !in_double_quote;
            } else if !in_single_quote && !in_double_quote {
                match c {
                    '(' => paren_depth += 1,
                    ')' => {
                        if paren_depth > 0 {
                            paren_depth -= 1;
                        }
                    }
                    '[' => bracket_depth += 1,
                    ']' => {
                        if bracket_depth > 0 {
                            bracket_depth -= 1;
                        }
                    }
                    '{' => brace_depth += 1,
                    '}' => {
                        if brace_depth > 0 {
                            brace_depth -= 1;
                        }
                    }
                    ',' => {
                        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
                            let element_str: String = chars[last_start..i].iter().collect();
                            list.push(parse_literal(&element_str)?);
                            last_start = i + 1;
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        if last_start < n {
            let element_str: String = chars[last_start..].iter().collect();
            list.push(parse_literal(&element_str)?);
        }
        Ok(Literal::List(list))
    } else if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        Ok(Literal::Str(trimmed[1..trimmed.len() - 1].to_string()))
    } else if trimmed.eq_ignore_ascii_case("true") {
        Ok(Literal::Bool(true))
    } else if trimmed.eq_ignore_ascii_case("false") {
        Ok(Literal::Bool(false))
    } else if trimmed.eq_ignore_ascii_case("null") {
        Ok(Literal::Null)
    } else if let Ok(val) = trimmed.parse::<i64>() {
        Ok(Literal::Int(val))
    } else if let Ok(val) = trimmed.parse::<f64>() {
        Ok(Literal::Float(val))
    } else {
        Err(format!("invalid literal: {}", s))
    }
}

fn parse_return_items(s: &str) -> Result<Vec<ReturnItem>, String> {
    let mut items = Vec::new();
    for part in s.split(',') {
        let trimmed = part.trim();
        let upper = trimmed.to_ascii_uppercase();
        let alias_part = upper.find(" AS ");

        let (expr_str, alias) = if let Some(idx) = alias_part {
            let expr = trimmed[..idx].trim();
            let alias_name = trimmed[idx + " AS ".len()..].trim().to_string();
            (expr, Some(alias_name))
        } else {
            (trimmed, None)
        };

        let expr = parse_expr(expr_str)?;
        items.push(ReturnItem { expr, alias });
    }
    Ok(items)
}

fn parse_return_clause(s: &str) -> Result<ReturnClause, String> {
    let items = parse_return_items(s)?;
    Ok(ReturnClause { items })
}
