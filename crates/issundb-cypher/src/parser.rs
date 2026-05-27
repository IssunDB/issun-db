use std::collections::HashMap;

use crate::ast::*;

/// Parse a Cypher query string into a `Statement` AST.
pub fn parse(cypher: &str) -> Result<Statement, String> {
    let normalized = cypher.trim();
    let upper = normalized.to_ascii_uppercase();

    if upper.starts_with("CREATE INDEX") {
        parse_create_index(normalized)
    } else if upper.starts_with("CREATE") {
        let pattern_str = normalized["CREATE".len()..].trim();
        let pattern = parse_pattern(pattern_str)?;
        Ok(Statement::Create(CreateStatement { pattern }))
    } else if upper.starts_with("DROP INDEX") {
        parse_drop_index(normalized)
    } else if upper.starts_with("MERGE") {
        parse_merge_statement(normalized)
    } else if upper.starts_with("MATCH") {
        if upper.contains(" SET ") {
            parse_set_statement(normalized)
        } else if upper.contains(" DELETE ") {
            parse_delete_statement(normalized)
        } else {
            let query = parse_read_query(normalized)?;
            Ok(Statement::Query(query))
        }
    } else if upper.starts_with("UNWIND")
        || upper.starts_with("WITH")
        || upper.starts_with("OPTIONAL MATCH")
        || upper.starts_with("RETURN")
    {
        // Queries that begin with UNWIND, WITH, OPTIONAL MATCH, or a bare RETURN
        // are read queries without a leading mandatory MATCH clause.
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

    // Find the RETURN clause (required) and optional ORDER BY / SKIP / LIMIT after it.
    let return_idx = boundaries
        .iter()
        .position(|(kw, _)| kw == "RETURN")
        .ok_or("Cypher read query must end with a RETURN clause")?;

    let (_, return_start) = &boundaries[return_idx];

    // Content between RETURN and the next recognised clause (ORDER BY / SKIP / LIMIT)
    // or end-of-string.
    let after_return_clauses = &boundaries[return_idx + 1..];

    // Determine where RETURN content ends.
    let return_content_end = after_return_clauses
        .first()
        .map(|(_, pos)| *pos)
        .unwrap_or(cypher.len());

    let return_content = cypher[*return_start + "RETURN".len()..return_content_end].trim();
    let return_clause = parse_return_clause(return_content)?;

    // Parse ORDER BY, SKIP, LIMIT from the trailing boundary clauses.
    let mut order_by: Option<OrderBy> = None;
    let mut skip_expr: Option<Expr> = None;
    let mut limit_expr: Option<Expr> = None;

    for i in 0..after_return_clauses.len() {
        let (kw, start) = &after_return_clauses[i];
        let end = after_return_clauses
            .get(i + 1)
            .map(|(_, p)| *p)
            .unwrap_or(cypher.len());
        let content = cypher[*start + kw.len()..end].trim();

        match kw.as_str() {
            "ORDER BY" => {
                order_by = Some(parse_order_by(content)?);
            }
            "SKIP" => {
                skip_expr = Some(parse_expr(content)?);
            }
            "LIMIT" => {
                limit_expr = Some(parse_expr(content)?);
            }
            other => {
                return Err(format!(
                    "unexpected trailing clause after RETURN: {}",
                    other
                ));
            }
        }
    }

    // Parse all intermediate clauses before RETURN.
    let pre_return_boundaries = &boundaries[..return_idx];
    let mut parts = Vec::new();

    for i in 0..pre_return_boundaries.len() {
        let (kw, start) = &pre_return_boundaries[i];
        let end = pre_return_boundaries
            .get(i + 1)
            .map(|(_, p)| *p)
            .unwrap_or(*return_start);
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
            "OPTIONAL MATCH" => {
                let (match_str, where_str) = split_optional_where(content);
                let match_clauses = parse_match_clauses(match_str)?;
                let where_clause = where_str.map(parse_where_clause).transpose()?;
                parts.push(QueryPart::OptionalMatch {
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
                    order_by: None,
                    skip: None,
                    limit: None,
                });
            }
            // ORDER BY, SKIP, and LIMIT can follow a WITH clause as an intermediate
            // clause. Attach them to the preceding WITH rather than treating them
            // as top-level boundaries.
            "ORDER BY" => match parts.last_mut() {
                Some(QueryPart::With { order_by, .. }) => {
                    *order_by = Some(parse_order_by(content)?);
                }
                _ => {
                    return Err("ORDER BY must follow a WITH or RETURN clause".to_string());
                }
            },
            "SKIP" => match parts.last_mut() {
                Some(QueryPart::With { skip, .. }) => {
                    *skip = Some(parse_expr(content)?);
                }
                _ => {
                    return Err("SKIP must follow a WITH or RETURN clause".to_string());
                }
            },
            "LIMIT" => match parts.last_mut() {
                Some(QueryPart::With { limit, .. }) => {
                    *limit = Some(parse_expr(content)?);
                }
                _ => {
                    return Err("LIMIT must follow a WITH or RETURN clause".to_string());
                }
            },
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

    // Validate that no variable is used as both a node and a relationship across
    // all MATCH / OPTIONAL MATCH clauses in the full query.
    validate_cross_clause_variable_types(&parts)?;

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
        order_by,
        skip: skip_expr,
        limit: limit_expr,
    })
}

fn parse_order_by(s: &str) -> Result<OrderBy, String> {
    let mut items = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        let upper = part.to_ascii_uppercase();
        let (expr_str, ascending) = if upper.ends_with(" DESC") {
            (&part[..part.len() - " DESC".len()], false)
        } else if upper.ends_with(" ASC") {
            (&part[..part.len() - " ASC".len()], true)
        } else {
            (part, true)
        };
        let expr = parse_expr(expr_str.trim())?;
        items.push(SortItem { expr, ascending });
    }
    Ok(OrderBy { items })
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
    //
    // ORDER BY is a two-word keyword; it is matched by scanning for "ORDER"
    // and checking that the next non-space token is "BY".
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
                // Try each single-word keyword first, then ORDER BY.
                let mut matched_kw: Option<&str> = None;
                for kw in &["MATCH", "WITH", "UNWIND", "RETURN", "SKIP", "LIMIT"] {
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

                // Check for two-word "ORDER BY".
                if matched_kw.is_none() {
                    let order_len = "ORDER".len();
                    if let Some(candidate) = cypher.get(byte_pos..byte_pos + order_len) {
                        if candidate.eq_ignore_ascii_case("ORDER") {
                            let is_start = ci == 0
                                || char_positions[ci - 1].1.is_ascii_whitespace()
                                || char_positions[ci - 1].1 == ';';
                            let after_order_ci = ci + order_len;
                            let is_end_order = after_order_ci >= n
                                || char_positions[after_order_ci].1.is_ascii_whitespace();
                            if is_start && is_end_order {
                                // Scan forward for "BY".
                                let mut scan_ci = after_order_ci;
                                while scan_ci < n && char_positions[scan_ci].1.is_ascii_whitespace()
                                {
                                    scan_ci += 1;
                                }
                                if scan_ci < n {
                                    let (by_byte, _) = char_positions[scan_ci];
                                    let by_end = by_byte + "BY".len();
                                    if by_end <= cypher_byte_len {
                                        if let Some(by_cand) = cypher.get(by_byte..by_end) {
                                            if by_cand.eq_ignore_ascii_case("BY") {
                                                let after_by_ci = scan_ci + "BY".len();
                                                let is_end_by = after_by_ci >= n
                                                    || char_positions[after_by_ci]
                                                        .1
                                                        .is_ascii_whitespace();
                                                if is_end_by {
                                                    // Keyword spans from byte_pos to end of "BY".
                                                    let kw_end_byte = by_end;
                                                    clauses
                                                        .push(("ORDER BY".to_string(), byte_pos));
                                                    // kw_end_byte is a byte offset; advance ci
                                                    // to the char index just past the keyword.
                                                    let skip_chars = cypher[byte_pos..kw_end_byte]
                                                        .chars()
                                                        .count();
                                                    ci += skip_chars;
                                                    continue;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Check for two-word "OPTIONAL MATCH".
                if matched_kw.is_none() {
                    let optional_len = "OPTIONAL".len();
                    if let Some(candidate) = cypher.get(byte_pos..byte_pos + optional_len) {
                        if candidate.eq_ignore_ascii_case("OPTIONAL") {
                            let is_start = ci == 0
                                || char_positions[ci - 1].1.is_ascii_whitespace()
                                || char_positions[ci - 1].1 == ';';
                            let after_optional_ci = ci + optional_len;
                            let is_end_optional = after_optional_ci >= n
                                || char_positions[after_optional_ci].1.is_ascii_whitespace();
                            if is_start && is_end_optional {
                                // Scan forward for "MATCH".
                                let mut scan_ci = after_optional_ci;
                                while scan_ci < n && char_positions[scan_ci].1.is_ascii_whitespace()
                                {
                                    scan_ci += 1;
                                }
                                if scan_ci < n {
                                    let (match_byte, _) = char_positions[scan_ci];
                                    let match_end = match_byte + "MATCH".len();
                                    if match_end <= cypher_byte_len {
                                        if let Some(match_cand) = cypher.get(match_byte..match_end)
                                        {
                                            if match_cand.eq_ignore_ascii_case("MATCH") {
                                                let after_match_ci = scan_ci + "MATCH".len();
                                                let is_end_match = after_match_ci >= n
                                                    || char_positions[after_match_ci]
                                                        .1
                                                        .is_ascii_whitespace()
                                                    || char_positions[after_match_ci].1 == '(';
                                                if is_end_match {
                                                    clauses.push((
                                                        "OPTIONAL MATCH".to_string(),
                                                        byte_pos,
                                                    ));
                                                    let kw_end_byte = match_end;
                                                    let skip_chars = cypher[byte_pos..kw_end_byte]
                                                        .chars()
                                                        .count();
                                                    ci += skip_chars;
                                                    continue;
                                                }
                                            }
                                        }
                                    }
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
    for part in split_by_comma_outside_braces(s) {
        let pattern = parse_pattern(part.trim())?;
        clauses.push(MatchClause { pattern });
    }
    validate_match_clause_variables(&clauses)?;
    Ok(clauses)
}

/// Validate that no relationship variable appears twice in the same MATCH clause
/// collection, and that no variable is used as both a node and a relationship.
/// The TCK requires a SyntaxError (VariableAlreadyBound or VariableTypeConflict) in
/// such cases.
fn validate_match_clause_variables(clauses: &[MatchClause]) -> Result<(), String> {
    let mut node_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rel_vars: std::collections::HashSet<String> = std::collections::HashSet::new();

    for clause in clauses {
        let pattern = &clause.pattern;
        // Collect seed node variable.
        if let Some(ref v) = pattern.node.variable {
            if rel_vars.contains(v) {
                return Err(format!(
                    "SyntaxError(VariableTypeConflict): variable '{}' is used as both a node and a relationship",
                    v
                ));
            }
            node_vars.insert(v.clone());
        }
        for (rel, target) in &pattern.rels {
            // Check relationship variable.
            if let Some(ref v) = rel.variable {
                if node_vars.contains(v) {
                    return Err(format!(
                        "SyntaxError(VariableTypeConflict): variable '{}' is used as both a node and a relationship",
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
            // Check target node variable for type conflict against relationship variables.
            if let Some(ref v) = target.variable {
                if rel_vars.contains(v) {
                    return Err(format!(
                        "SyntaxError(VariableTypeConflict): variable '{}' is used as both a node and a relationship",
                        v
                    ));
                }
                node_vars.insert(v.clone());
            }
        }
    }
    Ok(())
}

/// Validate variable type consistency across all MATCH and OPTIONAL MATCH clauses in the
/// query. A variable bound as a node in one clause must not appear as a relationship
/// variable in another clause, and vice versa. The TCK calls this VariableTypeConflict.
///
/// Note: WITH and UNWIND reset the variable scope, so validation restarts after each one.
fn validate_cross_clause_variable_types(parts: &[QueryPart]) -> Result<(), String> {
    let mut node_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rel_vars: std::collections::HashSet<String> = std::collections::HashSet::new();

    for part in parts {
        let clauses: Option<&[MatchClause]> = match part {
            QueryPart::Match { match_clauses, .. } => Some(match_clauses.as_slice()),
            QueryPart::OptionalMatch { match_clauses, .. } => Some(match_clauses.as_slice()),
            QueryPart::With { .. } | QueryPart::Unwind { .. } => {
                // WITH and UNWIND create a scope barrier; reset tracked variables.
                node_vars.clear();
                rel_vars.clear();
                None
            }
        };

        if let Some(clauses) = clauses {
            for clause in clauses {
                let pattern = &clause.pattern;
                if let Some(ref v) = pattern.node.variable {
                    if rel_vars.contains(v) {
                        return Err(format!(
                            "SyntaxError(VariableTypeConflict): variable '{}' is used as both a node and a relationship",
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
                        rel_vars.insert(v.clone());
                    }
                    if let Some(ref v) = target.variable {
                        if rel_vars.contains(v) {
                            return Err(format!(
                                "SyntaxError(VariableTypeConflict): variable '{}' is used as both a node and a relationship",
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

    // Handle bare `--` or `->` (no brackets).
    if !remainder.starts_with('[') {
        if remainder.starts_with("->") {
            // `->`  directed outgoing, no bracket
            let consumed = left_dash.len() + 2;
            return Ok((
                RelationshipPattern {
                    variable: None,
                    rel_type: None,
                    is_incoming: false,
                    is_undirected: false,
                    range: None,
                    properties: None,
                },
                consumed,
            ));
        } else if remainder.starts_with('-') {
            // `--`  undirected, no bracket
            let consumed = left_dash.len() + 1;
            return Ok((
                RelationshipPattern {
                    variable: None,
                    rel_type: None,
                    is_incoming: false,
                    is_undirected: true,
                    range: None,
                    properties: None,
                },
                consumed,
            ));
        } else {
            return Err("relationship pattern must contain type bracket '['".into());
        }
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

    // Undirected when neither incoming (`<-`) nor outgoing (`->`).
    let is_undirected = !is_incoming && !is_outgoing;

    // Parse properties map if present in relationship bracket
    let prop_start = content.find('{');
    let (content_body, properties) = if let Some(idx) = prop_start {
        let prop_str = &content[idx..];
        let props = parse_properties_map(prop_str)?;
        (&content[..idx].trim(), Some(props))
    } else {
        (&content, None)
    };

    let (left_part, range_part) = if let Some(star_idx) = content_body.find('*') {
        (
            content_body[..star_idx].trim(),
            Some(content_body[star_idx + 1..].trim()),
        )
    } else {
        (*content_body, None)
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
            is_undirected,
            range,
            properties,
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
    for item in split_by_comma_outside_braces(inner) {
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
    // Parse as a full expression first. Map simple comparison binary ops to their
    // specific WhereClause variants (for compatibility with the legacy planner path)
    // and wrap everything else as WhereClause::Expr.
    let expr = parse_expr(s)?;
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

/// Parse a Cypher expression. Entry point for full expression parsing with
/// operator precedence: OR < AND < NOT < IS NULL < comparison < additive < multiplicative < atom.
pub(crate) fn parse_expr(s: &str) -> Result<Expr, String> {
    parse_expr_or(s.trim())
}

fn parse_expr_or(s: &str) -> Result<Expr, String> {
    if let Some(idx) = find_keyword_op_at_root(s, " OR ") {
        let left = parse_expr_and(s[..idx].trim())?;
        let right = parse_expr_or(s[idx + 4..].trim())?;
        return Ok(Expr::BinaryOp {
            op: BinaryOperator::Or,
            left: Box::new(left),
            right: Box::new(right),
        });
    }
    parse_expr_and(s)
}

fn parse_expr_and(s: &str) -> Result<Expr, String> {
    if let Some(idx) = find_keyword_op_at_root(s, " AND ") {
        let left = parse_expr_not(s[..idx].trim())?;
        let right = parse_expr_and(s[idx + 5..].trim())?;
        return Ok(Expr::BinaryOp {
            op: BinaryOperator::And,
            left: Box::new(left),
            right: Box::new(right),
        });
    }
    parse_expr_not(s)
}

fn parse_expr_not(s: &str) -> Result<Expr, String> {
    let upper = s.to_ascii_uppercase();
    if upper.starts_with("NOT ") {
        let inner = parse_expr_not(s[4..].trim())?;
        return Ok(Expr::Not(Box::new(inner)));
    }
    parse_expr_cmp(s)
}

/// Scan for the leftmost comparison operator at root level.
/// Returns (byte_offset, operator_string).
/// Handles only BINARY comparison operators (excludes IS NULL / IS NOT NULL which are postfix).
/// Operators: CONTAINS, STARTS WITH, ENDS WITH, IN, NOT IN, <>, !=, <=, >=, =~, <, >, =
fn find_leftmost_cmp_op(s: &str) -> Option<(usize, &'static str)> {
    let mut best: Option<(usize, &'static str)> = None;

    let keyword_ops: &[&str] = &[
        " CONTAINS ",
        " STARTS WITH ",
        " ENDS WITH ",
        " NOT IN ",
        " IN ",
    ];
    for &kw in keyword_ops {
        if let Some(idx) = find_keyword_op_at_root(s, kw) {
            match best {
                None => best = Some((idx, kw)),
                Some((best_idx, _)) if idx < best_idx => best = Some((idx, kw)),
                _ => {}
            }
        }
    }

    let sym_ops: &[&str] = &["<>", "!=", "<=", ">=", "=~", "<", ">", "="];
    for &op in sym_ops {
        if let Some(idx) = find_sym_op_at_root(s, op) {
            match best {
                None => best = Some((idx, op)),
                Some((best_idx, _)) if idx < best_idx => best = Some((idx, op)),
                _ => {}
            }
        }
    }

    best
}

/// Parse IS NULL / IS NOT NULL as postfix; if absent, delegate to parse_expr_add.
/// This function does NOT call parse_expr_cmp to avoid mutual recursion.
fn parse_expr_isnull(s: &str) -> Result<Expr, String> {
    let upper = s.to_ascii_uppercase();
    if upper.ends_with(" IS NOT NULL") {
        let inner = parse_expr_add(s[..s.len() - " IS NOT NULL".len()].trim())?;
        return Ok(Expr::IsNotNull(Box::new(inner)));
    }
    if upper.ends_with(" IS NULL") {
        let inner = parse_expr_add(s[..s.len() - " IS NULL".len()].trim())?;
        return Ok(Expr::IsNull(Box::new(inner)));
    }
    parse_expr_add(s)
}

fn parse_expr_cmp(s: &str) -> Result<Expr, String> {
    // parse_expr_isnull is used for operands so that `a = b IS NULL` parses as `a = (b IS NULL)`.
    // IS NULL/NOT NULL at the top level are also handled via find_leftmost_cmp_op.
    match find_leftmost_cmp_op(s) {
        Some((idx, " IS NOT NULL")) => {
            let inner = parse_expr_add(s[..idx].trim())?;
            Ok(Expr::IsNotNull(Box::new(inner)))
        }
        Some((idx, " IS NULL")) => {
            let inner = parse_expr_add(s[..idx].trim())?;
            Ok(Expr::IsNull(Box::new(inner)))
        }
        Some((idx, " CONTAINS ")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let right = parse_expr_isnull(s[idx + " CONTAINS ".len()..].trim())?;
            Ok(Expr::FunctionCall {
                name: "__contains__".to_string(),
                args: vec![left, right],
            })
        }
        Some((idx, " STARTS WITH ")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let right = parse_expr_isnull(s[idx + " STARTS WITH ".len()..].trim())?;
            Ok(Expr::FunctionCall {
                name: "__starts_with__".to_string(),
                args: vec![left, right],
            })
        }
        Some((idx, " ENDS WITH ")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let right = parse_expr_isnull(s[idx + " ENDS WITH ".len()..].trim())?;
            Ok(Expr::FunctionCall {
                name: "__ends_with__".to_string(),
                args: vec![left, right],
            })
        }
        Some((idx, " IN ")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let right = parse_expr_isnull(s[idx + 4..].trim())?;
            Ok(Expr::FunctionCall {
                name: "__in__".to_string(),
                args: vec![left, right],
            })
        }
        Some((idx, " NOT IN ")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let right = parse_expr_isnull(s[idx + " NOT IN ".len()..].trim())?;
            let inner = Expr::FunctionCall {
                name: "__in__".to_string(),
                args: vec![left, right],
            };
            Ok(Expr::Not(Box::new(inner)))
        }
        Some((idx, "<>")) | Some((idx, "!=")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let op_len = 2usize;
            let right = parse_expr_isnull(s[idx + op_len..].trim())?;
            Ok(Expr::BinaryOp {
                op: BinaryOperator::Ne,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        Some((idx, "<=")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let right = parse_expr_isnull(s[idx + 2..].trim())?;
            Ok(Expr::BinaryOp {
                op: BinaryOperator::Le,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        Some((idx, ">=")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let right = parse_expr_isnull(s[idx + 2..].trim())?;
            Ok(Expr::BinaryOp {
                op: BinaryOperator::Ge,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        Some((idx, "=~")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let right = parse_expr_isnull(s[idx + 2..].trim())?;
            Ok(Expr::FunctionCall {
                name: "__regex__".to_string(),
                args: vec![left, right],
            })
        }
        Some((idx, "<")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let right = parse_expr_isnull(s[idx + 1..].trim())?;
            Ok(Expr::BinaryOp {
                op: BinaryOperator::Lt,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        Some((idx, ">")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let right = parse_expr_isnull(s[idx + 1..].trim())?;
            Ok(Expr::BinaryOp {
                op: BinaryOperator::Gt,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        Some((idx, "=")) => {
            let left = parse_expr_isnull(s[..idx].trim())?;
            let right = parse_expr_isnull(s[idx + 1..].trim())?;
            Ok(Expr::BinaryOp {
                op: BinaryOperator::Eq,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        _ => parse_expr_isnull(s),
    }
}

fn parse_expr_add(s: &str) -> Result<Expr, String> {
    if let Some((idx, op_ch)) = find_additive_at_root(s) {
        let left = parse_expr_add(s[..idx].trim())?;
        let right = parse_expr_mul(s[idx + 1..].trim())?;
        let op = if op_ch == '+' {
            BinaryOperator::Add
        } else {
            BinaryOperator::Sub
        };
        return Ok(Expr::BinaryOp {
            op,
            left: Box::new(left),
            right: Box::new(right),
        });
    }
    parse_expr_mul(s)
}

fn parse_expr_mul(s: &str) -> Result<Expr, String> {
    if let Some((idx, op_ch)) = find_multiplicative_at_root(s) {
        let left = parse_expr_mul(s[..idx].trim())?;
        let right = parse_expr_atom(s[idx + 1..].trim())?;
        let op = match op_ch {
            '*' => BinaryOperator::Mul,
            '/' => BinaryOperator::Div,
            '%' => BinaryOperator::Mod,
            _ => unreachable!(),
        };
        return Ok(Expr::BinaryOp {
            op,
            left: Box::new(left),
            right: Box::new(right),
        });
    }
    parse_expr_atom(s)
}

fn parse_expr_atom(s: &str) -> Result<Expr, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("empty expression".into());
    }
    // Unary minus: -(expr) or -literal
    if let Some(stripped) = trimmed.strip_prefix('-') {
        let rest = stripped.trim();
        // Only treat as unary if rest starts with '(' or is a number
        if rest.starts_with('(') || rest.parse::<f64>().is_ok() {
            let inner = parse_expr_atom(rest)?;
            return Ok(Expr::BinaryOp {
                op: BinaryOperator::Sub,
                left: Box::new(Expr::Literal(Literal::Int(0))),
                right: Box::new(inner),
            });
        }
    }
    if trimmed.starts_with('(') && trimmed.ends_with(')') && expr_parens_balanced(trimmed) {
        return parse_expr_or(&trimmed[1..trimmed.len() - 1]);
    }
    if trimmed.starts_with('[') && trimmed.ends_with(']') && expr_brackets_balanced(trimmed) {
        return parse_list_expr(trimmed);
    }
    if trimmed.starts_with('{') && trimmed.ends_with('}') && expr_braces_balanced(trimmed) {
        return parse_map_expr(trimmed);
    }
    if let Some(rest) = trimmed.strip_prefix('$') {
        return Ok(Expr::Param(rest.to_string()));
    }
    if let Some(expr) = try_parse_agg(trimmed)? {
        return Ok(expr);
    }
    if let Some(expr) = try_parse_quantifier_expr(trimmed)? {
        return Ok(expr);
    }
    if let Ok(lit) = parse_literal(trimmed) {
        return Ok(Expr::Literal(lit));
    }
    if let Some(expr) = try_parse_fn_call(trimmed)? {
        return Ok(expr);
    }
    if let Some(dot_pos) = trimmed.rfind('.') {
        let left_part = trimmed[..dot_pos].trim();
        let right_part = trimmed[dot_pos + 1..].trim();
        let left_ok =
            !left_part.is_empty() && left_part.chars().all(|c| c.is_alphanumeric() || c == '_');
        let right_ok =
            !right_part.is_empty() && right_part.chars().all(|c| c.is_alphanumeric() || c == '_');
        if left_ok && right_ok {
            return Ok(Expr::Prop(left_part.to_string(), right_part.to_string()));
        }
    }
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Ok(Expr::Prop(trimmed.to_string(), "".to_string()));
    }
    Err(format!("invalid expression: {}", trimmed))
}

struct RootScanner {
    paren: i32,
    bracket: i32,
    brace: i32,
    in_sq: bool,
    in_dq: bool,
}

impl RootScanner {
    fn new() -> Self {
        RootScanner {
            paren: 0,
            bracket: 0,
            brace: 0,
            in_sq: false,
            in_dq: false,
        }
    }
    fn feed(&mut self, c: char) {
        if c == '\'' && !self.in_dq {
            self.in_sq = !self.in_sq;
            return;
        }
        if c == '"' && !self.in_sq {
            self.in_dq = !self.in_dq;
            return;
        }
        if self.in_sq || self.in_dq {
            return;
        }
        match c {
            '(' => self.paren += 1,
            ')' => self.paren -= 1,
            '[' => self.bracket += 1,
            ']' => self.bracket -= 1,
            '{' => self.brace += 1,
            '}' => self.brace -= 1,
            _ => {}
        }
    }
    fn at_root(&self) -> bool {
        self.paren == 0 && self.bracket == 0 && self.brace == 0 && !self.in_sq && !self.in_dq
    }
}

fn char_byte_positions(s: &str) -> (Vec<char>, Vec<usize>) {
    let chars: Vec<char> = s.chars().collect();
    let mut byte_pos: Vec<usize> = Vec::with_capacity(chars.len() + 1);
    let mut bp = 0usize;
    for &ch in &chars {
        byte_pos.push(bp);
        bp += ch.len_utf8();
    }
    byte_pos.push(bp);
    (chars, byte_pos)
}

fn find_keyword_op_at_root(s: &str, kw: &str) -> Option<usize> {
    let upper = s.to_ascii_uppercase();
    let kw_upper = kw.to_ascii_uppercase();
    let kw_len = kw.len();
    let n = s.len();
    if n < kw_len {
        return None;
    }
    let (chars, byte_pos) = char_byte_positions(s);
    let mut sc = RootScanner::new();
    for (ci, &ch) in chars.iter().enumerate() {
        let bp = byte_pos[ci];
        if sc.at_root() && bp + kw_len <= n && upper[bp..].starts_with(&kw_upper[..]) {
            return Some(bp);
        }
        sc.feed(ch);
    }
    None
}

fn find_sym_op_at_root(s: &str, op: &str) -> Option<usize> {
    let op_bytes = op.as_bytes();
    let op_len = op.len();
    let s_bytes = s.as_bytes();
    let n = s_bytes.len();
    if n < op_len {
        return None;
    }
    let (chars, byte_pos) = char_byte_positions(s);
    let mut sc = RootScanner::new();
    for (ci, &ch) in chars.iter().enumerate() {
        let bp = byte_pos[ci];
        if sc.at_root() && bp + op_len <= n && &s_bytes[bp..bp + op_len] == op_bytes {
            // Avoid matching "=" when preceded by '<', '>', '!', '=' (those are two-char ops)
            if (op == "=" || op == "<" || op == ">")
                && bp > 0
                && matches!(s_bytes[bp - 1], b'<' | b'>' | b'!' | b'=')
            {
                sc.feed(ch);
                continue;
            }
            // Avoid matching "<" or ">" when followed by "=" (those are two-char ops)
            if (op == "<" || op == ">") && bp + 1 < n && s_bytes[bp + 1] == b'=' {
                sc.feed(ch);
                continue;
            }
            // Avoid matching "<" when it's "<>"
            if op == "<" && bp + 1 < n && s_bytes[bp + 1] == b'>' {
                sc.feed(ch);
                continue;
            }
            return Some(bp);
        }
        sc.feed(ch);
    }
    None
}

fn find_additive_at_root(s: &str) -> Option<(usize, char)> {
    let (chars, byte_pos) = char_byte_positions(s);
    let mut sc = RootScanner::new();
    let mut result: Option<(usize, char)> = None;
    for (ci, &ch) in chars.iter().enumerate() {
        let bp = byte_pos[ci];
        if sc.at_root() && (ch == '+' || ch == '-') && ci > 0 {
            let prev_non_space = chars[..ci].iter().rposition(|&c| c != ' ' && c != '\t');
            if let Some(pci) = prev_non_space {
                let pc = chars[pci];
                if pc.is_alphanumeric()
                    || pc == '_'
                    || pc == ')'
                    || pc == ']'
                    || pc == '\''
                    || pc == '"'
                {
                    result = Some((bp, ch));
                }
            }
        }
        sc.feed(ch);
    }
    result
}

fn find_multiplicative_at_root(s: &str) -> Option<(usize, char)> {
    let (chars, byte_pos) = char_byte_positions(s);
    let mut sc = RootScanner::new();
    let mut result: Option<(usize, char)> = None;
    for (ci, &ch) in chars.iter().enumerate() {
        let bp = byte_pos[ci];
        if sc.at_root() && (ch == '*' || ch == '/' || ch == '%') {
            result = Some((bp, ch));
        }
        sc.feed(ch);
    }
    result
}

fn expr_parens_balanced(s: &str) -> bool {
    if !s.starts_with('(') {
        return false;
    }
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut sc = RootScanner::new();
    for (i, &ch) in chars.iter().enumerate() {
        sc.feed(ch);
        if i > 0 && i < n - 1 && sc.at_root() {
            return false;
        }
    }
    sc.at_root()
}

fn expr_brackets_balanced(s: &str) -> bool {
    if !s.starts_with('[') {
        return false;
    }
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut sc = RootScanner::new();
    for (i, &ch) in chars.iter().enumerate() {
        sc.feed(ch);
        if i > 0 && i < n - 1 && sc.at_root() {
            return false;
        }
    }
    sc.at_root()
}

fn expr_braces_balanced(s: &str) -> bool {
    if !s.starts_with('{') {
        return false;
    }
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut sc = RootScanner::new();
    for (i, &ch) in chars.iter().enumerate() {
        sc.feed(ch);
        if i > 0 && i < n - 1 && sc.at_root() {
            return false;
        }
    }
    sc.at_root()
}

fn split_expr_args(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let (chars, byte_pos) = char_byte_positions(s);
    let mut start = 0usize;
    let mut sc = RootScanner::new();
    for (ci, &ch) in chars.iter().enumerate() {
        let bp = byte_pos[ci];
        if sc.at_root() && ch == ',' {
            result.push(s[start..bp].trim());
            start = byte_pos[ci + 1];
        }
        sc.feed(ch);
    }
    result.push(s[start..].trim());
    result
}

fn parse_list_expr(s: &str) -> Result<Expr, String> {
    let inner = s[1..s.len() - 1].trim();
    if inner.is_empty() {
        return Ok(Expr::FunctionCall {
            name: "__list__".to_string(),
            args: vec![],
        });
    }
    let parts = split_expr_args(inner);
    let mut args = Vec::with_capacity(parts.len());
    for part in parts {
        args.push(parse_expr_or(part.trim())?);
    }
    Ok(Expr::FunctionCall {
        name: "__list__".to_string(),
        args,
    })
}

fn parse_map_expr(s: &str) -> Result<Expr, String> {
    let inner = s[1..s.len() - 1].trim();
    if inner.is_empty() {
        return Ok(Expr::FunctionCall {
            name: "__map__".to_string(),
            args: vec![],
        });
    }
    let parts = split_expr_args(inner);
    let mut args = Vec::with_capacity(parts.len() * 2);
    for part in parts {
        let colon_pos =
            find_root_colon(part).ok_or_else(|| format!("invalid map entry: {}", part))?;
        let key = part[..colon_pos].trim();
        let val_str = part[colon_pos + 1..].trim();
        args.push(Expr::Literal(Literal::Str(key.to_string())));
        args.push(parse_expr_or(val_str)?);
    }
    Ok(Expr::FunctionCall {
        name: "__map__".to_string(),
        args,
    })
}

fn find_root_colon(s: &str) -> Option<usize> {
    let (chars, byte_pos) = char_byte_positions(s);
    let mut sc = RootScanner::new();
    for (ci, &ch) in chars.iter().enumerate() {
        if sc.at_root() && ch == ':' {
            return Some(byte_pos[ci]);
        }
        sc.feed(ch);
    }
    None
}

fn try_parse_quantifier_expr(s: &str) -> Result<Option<Expr>, String> {
    let upper = s.to_ascii_uppercase();
    let kind = if upper.starts_with("ALL(") {
        QuantifierKind::All
    } else if upper.starts_with("ANY(") {
        QuantifierKind::Any
    } else if upper.starts_with("NONE(") {
        QuantifierKind::None
    } else if upper.starts_with("SINGLE(") {
        QuantifierKind::Single
    } else {
        return Ok(None);
    };
    if !s.ends_with(')') {
        return Ok(None);
    }
    let fn_len = match kind {
        QuantifierKind::All | QuantifierKind::Any => 4,
        QuantifierKind::None => 5,
        QuantifierKind::Single => 7,
    };
    let inner = s[fn_len..s.len() - 1].trim();
    let inner_upper = inner.to_ascii_uppercase();
    let in_pos = match inner_upper.find(" IN ") {
        Some(p) => p,
        None => return Ok(None),
    };
    let where_pos = match inner_upper.find(" WHERE ") {
        Some(p) => p,
        None => return Ok(None),
    };
    let variable = inner[..in_pos].trim().to_string();
    let list_str = inner[in_pos + 4..where_pos].trim();
    let pred_str = inner[where_pos + 7..].trim();
    let list = parse_expr_or(list_str)?;
    let predicate = parse_expr_or(pred_str)?;
    Ok(Some(Expr::Quantifier {
        kind,
        variable,
        list: Box::new(list),
        predicate: Box::new(predicate),
    }))
}

fn try_parse_fn_call(s: &str) -> Result<Option<Expr>, String> {
    let paren_pos = match s.find('(') {
        Some(p) => p,
        None => return Ok(None),
    };
    if !s.ends_with(')') {
        return Ok(None);
    }
    let fn_name = s[..paren_pos].trim();
    if fn_name.is_empty()
        || !fn_name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
    {
        return Ok(None);
    }
    let args_str = s[paren_pos + 1..s.len() - 1].trim();
    if args_str.is_empty() {
        return Ok(Some(Expr::FunctionCall {
            name: fn_name.to_ascii_lowercase(),
            args: vec![],
        }));
    }
    let parts = split_expr_args(args_str);
    let mut args = Vec::with_capacity(parts.len());
    for part in parts {
        args.push(parse_expr_or(part.trim())?);
    }
    Ok(Some(Expr::FunctionCall {
        name: fn_name.to_ascii_lowercase(),
        args,
    }))
}

/// Attempt to parse an aggregate function call expression. Returns `Ok(None)` if the
/// input does not look like an aggregate call.
fn try_parse_agg(s: &str) -> Result<Option<Expr>, String> {
    // count(*) special case.
    if s.eq_ignore_ascii_case("count(*)") {
        return Ok(Some(Expr::CountStar));
    }

    // Generic `fn_name(inner)` or `fn_name(DISTINCT inner)`.
    let paren_open = match s.find('(') {
        Some(i) => i,
        None => return Ok(None),
    };
    if !s.ends_with(')') {
        return Ok(None);
    }

    let fn_name = s[..paren_open].trim();
    let inner_raw = s[paren_open + 1..s.len() - 1].trim();

    let agg_fn = match fn_name.to_ascii_uppercase().as_str() {
        "COUNT" => AggFn::Count { distinct: false },
        "SUM" => AggFn::Sum,
        "AVG" => AggFn::Avg,
        "MIN" => AggFn::Min,
        "MAX" => AggFn::Max,
        "COLLECT" => AggFn::Collect,
        _ => return Ok(None),
    };

    // Handle COUNT(DISTINCT expr).
    let (agg_fn, inner_str) =
        if matches!(agg_fn, AggFn::Count { .. }) && inner_raw.len() > "DISTINCT ".len() {
            let upper_inner = inner_raw.to_ascii_uppercase();
            if upper_inner.starts_with("DISTINCT ") {
                (
                    AggFn::Count { distinct: true },
                    inner_raw["DISTINCT ".len()..].trim(),
                )
            } else {
                (agg_fn, inner_raw)
            }
        } else {
            (agg_fn, inner_raw)
        };

    let inner_expr = parse_expr(inner_str)?;
    Ok(Some(Expr::Agg(agg_fn, Box::new(inner_expr))))
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
    for part in split_by_comma_outside_braces(s) {
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

fn parse_merge_statement(cypher: &str) -> Result<Statement, String> {
    // MERGE (pattern) [ON CREATE SET ...] [ON MATCH SET ...]
    let upper = cypher.to_ascii_uppercase();

    // Find "ON CREATE SET" and "ON MATCH SET" positions (case-insensitive).
    let on_create_pos = upper.find("ON CREATE SET");
    let on_match_pos = upper.find("ON MATCH SET");

    // Pattern content is from after "MERGE" to the first ON ... SET or end.
    let pattern_end = [on_create_pos, on_match_pos]
        .iter()
        .filter_map(|&p| p)
        .min()
        .unwrap_or(cypher.len());

    let pattern_str = cypher["MERGE".len()..pattern_end].trim();
    let pattern = parse_pattern(pattern_str)?;

    // Parse ON CREATE SET items.
    let on_create_set = if let Some(start) = on_create_pos {
        let content_start = start + "ON CREATE SET".len();
        let content_end = on_match_pos.filter(|&p| p > start).unwrap_or(cypher.len());
        parse_set_items_list(cypher[content_start..content_end].trim())?
    } else {
        Vec::new()
    };

    // Parse ON MATCH SET items.
    let on_match_set = if let Some(start) = on_match_pos {
        let content_start = start + "ON MATCH SET".len();
        let content_end = on_create_pos.filter(|&p| p > start).unwrap_or(cypher.len());
        parse_set_items_list(cypher[content_start..content_end].trim())?
    } else {
        Vec::new()
    };

    Ok(Statement::Merge(MergeStatement {
        pattern,
        on_create_set,
        on_match_set,
    }))
}

fn parse_set_items_list(s: &str) -> Result<Vec<SetItem>, String> {
    let mut items = Vec::new();
    for item_str in s.split(',') {
        let item_str = item_str.trim();
        if item_str.is_empty() {
            continue;
        }
        let parts: Vec<&str> = item_str.splitn(2, '=').map(|s| s.trim()).collect();
        if parts.len() != 2 {
            return Err(format!("invalid SET assignment: {}", item_str));
        }
        let prop_parts: Vec<&str> = parts[0].split('.').map(|s| s.trim()).collect();
        if prop_parts.len() != 2 {
            return Err(format!("invalid SET property reference: {}", parts[0]));
        }
        let variable = prop_parts[0].to_string();
        let property = prop_parts[1].to_string();
        let expr = parse_expr(parts[1])?;
        items.push(SetItem {
            variable,
            property,
            expr,
        });
    }
    Ok(items)
}

fn parse_create_index(cypher: &str) -> Result<Statement, String> {
    // CREATE INDEX FOR (n:Label) ON (n.property)
    let upper = cypher.to_ascii_uppercase();
    let for_pos = upper.find("FOR").ok_or("CREATE INDEX missing FOR")?;
    let on_pos = upper.find(" ON ").ok_or("CREATE INDEX missing ON")?;

    let for_content = cypher[for_pos + "FOR".len()..on_pos].trim();
    let on_content = cypher[on_pos + " ON ".len()..].trim();

    let label = parse_label_from_node_pattern(for_content)?;
    let property = parse_property_from_node_pattern(on_content)?;

    Ok(Statement::CreateIndex(CreateIndexStatement {
        label,
        property,
    }))
}

fn parse_drop_index(cypher: &str) -> Result<Statement, String> {
    // DROP INDEX FOR (n:Label) ON (n.property)
    let upper = cypher.to_ascii_uppercase();
    let for_pos = upper.find("FOR").ok_or("DROP INDEX missing FOR")?;
    let on_pos = upper.find(" ON ").ok_or("DROP INDEX missing ON")?;

    let for_content = cypher[for_pos + "FOR".len()..on_pos].trim();
    let on_content = cypher[on_pos + " ON ".len()..].trim();

    let label = parse_label_from_node_pattern(for_content)?;
    let property = parse_property_from_node_pattern(on_content)?;

    Ok(Statement::DropIndex(DropIndexStatement { label, property }))
}

/// Extract the label name from a node pattern like `(n:Label)` or `(:Label)`.
fn parse_label_from_node_pattern(s: &str) -> Result<String, String> {
    let inner = s
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    let colon_pos = inner
        .find(':')
        .ok_or_else(|| format!("no label in node pattern: {}", s))?;
    Ok(inner[colon_pos + 1..].trim().to_string())
}

/// Extract the property name from a node pattern like `(n.property)` or `(n.prop)`.
fn parse_property_from_node_pattern(s: &str) -> Result<String, String> {
    let inner = s
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    let dot_pos = inner
        .find('.')
        .ok_or_else(|| format!("no property in node pattern: {}", s))?;
    Ok(inner[dot_pos + 1..].trim().to_string())
}

fn split_by_comma_outside_braces(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut paren_count = 0;
    let mut bracket_count = 0;
    let mut brace_count = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let chars: Vec<char> = s.chars().collect();
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
                '(' => paren_count += 1,
                ')' => {
                    if paren_count > 0 {
                        paren_count -= 1;
                    }
                }
                '[' => bracket_count += 1,
                ']' => {
                    if bracket_count > 0 {
                        bracket_count -= 1;
                    }
                }
                '{' => brace_count += 1,
                '}' => {
                    if brace_count > 0 {
                        brace_count -= 1;
                    }
                }
                ',' => {
                    if paren_count == 0 && bracket_count == 0 && brace_count == 0 {
                        let part_str: String = chars[start..i].iter().collect();
                        parts.push(part_str);
                        start = i + 1;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    if start <= n {
        let part_str: String = chars[start..].iter().collect();
        parts.push(part_str);
    }
    parts
}

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
