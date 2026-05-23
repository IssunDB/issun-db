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
    } else {
        Err(format!("unsupported statement type in query: {}", cypher))
    }
}

fn parse_read_query(cypher: &str) -> Result<Query, String> {
    let upper = cypher.to_ascii_uppercase();
    let match_idx = upper.find("MATCH").ok_or("missing MATCH clause")?;
    let return_idx = upper.find("RETURN").ok_or("missing RETURN clause")?;

    let match_clause_str = if let Some(where_idx) = upper.find("WHERE") {
        if where_idx > match_idx && where_idx < return_idx {
            &cypher[match_idx + "MATCH".len()..where_idx]
        } else {
            return Err("WHERE clause must be placed between MATCH and RETURN".into());
        }
    } else {
        &cypher[match_idx + "MATCH".len()..return_idx]
    };

    let match_clauses = parse_match_clauses(match_clause_str)?;

    let where_clause = if let Some(where_idx) = upper.find("WHERE") {
        let where_clause_str = &cypher[where_idx + "WHERE".len()..return_idx];
        Some(parse_where_clause(where_clause_str)?)
    } else {
        None
    };

    let return_clause_str = &cypher[return_idx + "RETURN".len()..];
    let return_clause = parse_return_clause(return_clause_str)?;

    Ok(Query {
        match_clauses,
        where_clause,
        return_clause,
    })
}

fn parse_set_statement(cypher: &str) -> Result<Statement, String> {
    let upper = cypher.to_ascii_uppercase();
    let match_idx = upper.find("MATCH").ok_or("missing MATCH clause")?;
    let set_idx = upper.find("SET").ok_or("missing SET clause")?;

    let match_clause_str = if let Some(where_idx) = upper.find("WHERE") {
        if where_idx > match_idx && where_idx < set_idx {
            &cypher[match_idx + "MATCH".len()..where_idx]
        } else {
            return Err("WHERE clause must be placed between MATCH and SET".into());
        }
    } else {
        &cypher[match_idx + "MATCH".len()..set_idx]
    };

    let match_clauses = parse_match_clauses(match_clause_str)?;

    let where_clause = if let Some(where_idx) = upper.find("WHERE") {
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
    let upper = cypher.to_ascii_uppercase();
    let match_idx = upper.find("MATCH").ok_or("missing MATCH clause")?;
    let delete_idx = upper.find("DELETE").ok_or("missing DELETE clause")?;

    let match_clause_str = if let Some(where_idx) = upper.find("WHERE") {
        if where_idx > match_idx && where_idx < delete_idx {
            &cypher[match_idx + "MATCH".len()..where_idx]
        } else {
            return Err("WHERE clause must be placed between MATCH and DELETE".into());
        }
    } else {
        &cypher[match_idx + "MATCH".len()..delete_idx]
    };

    let match_clauses = parse_match_clauses(match_clause_str)?;

    let where_clause = if let Some(where_idx) = upper.find("WHERE") {
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
    remainder = &remainder[size..].trim();

    let mut rels = Vec::new();
    while !remainder.is_empty() {
        let (rel, rel_size) = parse_relationship_pattern(remainder)?;
        remainder = &remainder[rel_size..].trim();

        let (target, target_size) = parse_node_pattern(remainder)?;
        remainder = &remainder[target_size..].trim();

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
    let bracket_end = remainder.find(']').ok_or("missing relationship bracket ']'")?;
    let content = &remainder[1..bracket_end].trim();
    remainder = &remainder[bracket_end + 1..];

    let is_outgoing = remainder.starts_with("->");
    let right_dash = if is_outgoing { "->" } else { "-" };
    if !remainder.starts_with(right_dash) {
        return Err("invalid relationship syntax suffix".into());
    }

    let parts: Vec<&str> = content.split(':').collect();
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

    let consumed = left_dash.len() + bracket_end + 1 + right_dash.len();
    Ok((
        RelationshipPattern {
            variable,
            rel_type,
            is_incoming,
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
    for &(op, len) in &[("=", 1), ("!=", 2), ("<=", 2), (">=", 2), ("<", 1), (">", 1)] {
        if let Some(idx) = s.find(op) {
            if op_start.is_none() || idx < op_start.unwrap() {
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
    if trimmed.starts_with('$') {
        Ok(Expr::Param(trimmed[1..].to_string()))
    } else if trimmed.contains('.') {
        // Variable property reference
        let parts: Vec<&str> = trimmed.split('.').collect();
        if parts.len() == 2 {
            Ok(Expr::Prop(parts[0].trim().to_string(), parts[1].trim().to_string()))
        } else {
            Ok(Expr::Literal(parse_literal(trimmed)?))
        }
    } else {
        Ok(Expr::Literal(parse_literal(trimmed)?))
    }
}

fn parse_literal(s: &str) -> Result<Literal, String> {
    let trimmed = s.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') {
        Ok(Literal::Str(trimmed[1..trimmed.len() - 1].to_string()))
    } else if trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        Ok(Literal::Str(trimmed[1..trimmed.len() - 1].to_string()))
    } else if trimmed.to_ascii_lowercase() == "true" {
        Ok(Literal::Bool(true))
    } else if trimmed.to_ascii_lowercase() == "false" {
        Ok(Literal::Bool(false))
    } else if trimmed.to_ascii_lowercase() == "null" {
        Ok(Literal::Null)
    } else if let Ok(val) = trimmed.parse::<i64>() {
        Ok(Literal::Int(val))
    } else if let Ok(val) = trimmed.parse::<f64>() {
        Ok(Literal::Float(val))
    } else {
        Err(format!("invalid literal: {}", s))
    }
}

fn parse_return_clause(s: &str) -> Result<ReturnClause, String> {
    let mut items = Vec::new();
    for part in s.split(',') {
        let trimmed = part.trim();
        // Parse alias "AS" if present
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
    Ok(ReturnClause { items })
}
