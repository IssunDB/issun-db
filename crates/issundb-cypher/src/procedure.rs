//! Procedure registry for the `CALL` clause.
//!
//! IssunDB has no built-in stored procedures; this registry exists so callers
//! (and the openCypher TCK harness) can register table-backed procedures that
//! `CALL` can invoke. A procedure is modeled as a static relation whose columns
//! are `inputs ++ outputs`: calling it filters the rows whose input cells equal
//! the supplied arguments and yields the output cells.
//!
//! The registry holds no runtime graph state and is owned by the query layer,
//! so it does not violate the `issundb-core` boundary (procedures are a Cypher
//! concept, not a storage concept).

use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// The declared Cypher type of a procedure argument or output field.
///
/// Parsed from a signature fragment such as `INTEGER?`; the trailing `?`
/// nullability marker is ignored because every value may be null.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CypherType {
    Boolean,
    Integer,
    Float,
    Number,
    String,
    /// Any other or unrecognized declared type; accepts every value.
    Any,
}

impl CypherType {
    /// Parse a declared type fragment such as `STRING?` or `INTEGER`.
    pub fn parse(raw: &str) -> CypherType {
        let t = raw.trim().trim_end_matches('?').trim();
        match t.to_ascii_uppercase().as_str() {
            "BOOLEAN" => CypherType::Boolean,
            "INTEGER" => CypherType::Integer,
            "FLOAT" => CypherType::Float,
            "NUMBER" => CypherType::Number,
            "STRING" => CypherType::String,
            _ => CypherType::Any,
        }
    }

    /// Whether `value` is assignable to this declared type. Null is assignable to
    /// every type (all procedure types are nullable), and `NUMBER` accepts both
    /// integers and floats.
    pub fn accepts(&self, value: &Value) -> bool {
        match (self, value) {
            (_, Value::Null) => true,
            (CypherType::Any, _) => true,
            (CypherType::Boolean, Value::Bool(_)) => true,
            (CypherType::Integer, Value::Number(n)) => n.is_i64() || n.is_u64(),
            (CypherType::Float, Value::Number(_)) => true,
            (CypherType::Number, Value::Number(_)) => true,
            (CypherType::String, Value::String(_)) => true,
            _ => false,
        }
    }
}

/// A table-backed procedure. `rows` each contain `inputs.len() + outputs.len()`
/// cells: the first segment matches the declared inputs, the second the outputs.
#[derive(Debug, Clone)]
pub struct Procedure {
    pub name: String,
    pub inputs: Vec<(String, CypherType)>,
    pub outputs: Vec<(String, CypherType)>,
    pub rows: Vec<Vec<Value>>,
}

/// The output of resolving a `CALL` against the registry: the variable names a
/// row introduces and the concrete rows (one inner vec per produced row, aligned
/// with `output_vars`). A void procedure yields `output_vars == []` and a single
/// empty row so that an in-query call passes incoming rows through unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCall {
    pub output_vars: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// Compare a procedure input cell to a supplied argument, treating integers and
/// floats of equal numeric value as equal (so `42` matches a `FLOAT` cell `42.0`).
fn value_matches(cell: &Value, arg: &Value) -> bool {
    match (cell, arg) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => cell == arg,
    }
}

/// A runtime registry of procedures available to `CALL`.
#[derive(Debug, Clone, Default)]
pub struct ProcedureRegistry {
    procs: HashMap<String, Procedure>,
}

impl ProcedureRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, proc: Procedure) {
        self.procs.insert(proc.name.clone(), proc);
    }

    pub fn get(&self, name: &str) -> Option<&Procedure> {
        self.procs.get(name)
    }

    /// Resolve a `CALL` invocation into the concrete rows it produces.
    ///
    /// `args` are the evaluated explicit arguments (empty when `implicit` is
    /// true). `in_query` is true when the call appears in a larger query rather
    /// than standalone, which constrains the implicit-argument and `YIELD *`
    /// forms. `yields` carries explicit `YIELD field [AS alias]` items (`None`
    /// when no `YIELD` clause is present); `yield_star` is `YIELD *`.
    /// `already_bound` is the set of variables in scope before the call, used to
    /// reject `YIELD` renames that collide with existing bindings.
    ///
    /// Errors are returned as strings carrying the openCypher error name so the
    /// caller can surface them as compile-time `SyntaxError`s.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        &self,
        name: &str,
        args: &[Value],
        implicit: bool,
        in_query: bool,
        yields: &Option<Vec<(String, Option<String>)>>,
        yield_star: bool,
        already_bound: &HashSet<String>,
        params: &HashMap<String, Value>,
    ) -> Result<ResolvedCall, String> {
        let proc = self.get(name).ok_or_else(|| {
            format!(
                "SyntaxError(ProcedureNotFound): procedure `{}` not found",
                name
            )
        })?;
        resolve_against(
            proc,
            args,
            implicit,
            in_query,
            yields,
            yield_star,
            already_bound,
            params,
        )
    }
}

/// Resolve a `CALL` against a concrete [`Procedure`] (already looked up in a
/// registry or synthesized on the fly by a built-in). This holds the shared
/// argument validation, table filtering, and `YIELD` projection logic so that
/// the table-backed registry and the built-in graph-algorithm procedures behave
/// identically. See [`ProcedureRegistry::resolve`] for the argument meanings.
#[allow(clippy::too_many_arguments)]
pub fn resolve_against(
    proc: &Procedure,
    args: &[Value],
    implicit: bool,
    in_query: bool,
    yields: &Option<Vec<(String, Option<String>)>>,
    yield_star: bool,
    already_bound: &HashSet<String>,
    params: &HashMap<String, Value>,
) -> Result<ResolvedCall, String> {
    let name = proc.name.as_str();
    {
        // Determine the effective argument tuple.
        let effective_args: Vec<Value> = if implicit {
            if in_query && !proc.inputs.is_empty() {
                return Err(
                    "SyntaxError(InvalidArgumentPassingMode): procedure arguments must be passed \
                     explicitly in an in-query call"
                        .to_string(),
                );
            }
            // Implicit arguments are read from query parameters by input name; a
            // missing parameter is a compile-time error.
            let mut values = Vec::with_capacity(proc.inputs.len());
            for (arg_name, _) in &proc.inputs {
                match params.get(arg_name) {
                    Some(v) => values.push(v.clone()),
                    None => {
                        return Err(format!(
                            "ParameterMissing(MissingParameter): parameter `{}` for procedure \
                             `{}` was not provided",
                            arg_name, name
                        ));
                    }
                }
            }
            values
        } else {
            if args.len() != proc.inputs.len() {
                return Err(format!(
                    "SyntaxError(InvalidNumberOfArguments): procedure `{}` takes {} argument(s) \
                     but {} given",
                    name,
                    proc.inputs.len(),
                    args.len()
                ));
            }
            for (value, (_, ty)) in args.iter().zip(proc.inputs.iter()) {
                if !ty.accepts(value) {
                    return Err(format!(
                        "SyntaxError(InvalidArgumentType): argument to procedure `{}` has the \
                         wrong type",
                        name
                    ));
                }
            }
            args.to_vec()
        };

        let input_len = proc.inputs.len();

        // Filter the table to rows whose input cells equal the supplied arguments,
        // keeping only the output segment of each matching row. Argument matching
        // is numeric-aware so that an INTEGER value (e.g. `42`) matches a FLOAT
        // input cell (`42.0`).
        let matched_outputs: Vec<Vec<Value>> = proc
            .rows
            .iter()
            .filter(|row| {
                row.len() >= input_len
                    && row[..input_len]
                        .iter()
                        .zip(effective_args.iter())
                        .all(|(cell, arg)| value_matches(cell, arg))
            })
            .map(|row| row[input_len..].to_vec())
            .collect();

        // Determine which output fields to project and the variable names they bind.
        let (selected_indices, output_vars): (Vec<usize>, Vec<String>) = if yield_star {
            if in_query {
                return Err(
                    "SyntaxError(UnexpectedSyntax): YIELD * is not allowed in an in-query call"
                        .to_string(),
                );
            }
            (
                (0..proc.outputs.len()).collect(),
                proc.outputs.iter().map(|(n, _)| n.clone()).collect(),
            )
        } else if let Some(items) = yields {
            let mut indices = Vec::new();
            let mut vars = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for (field, alias) in items {
                let idx = proc
                    .outputs
                    .iter()
                    .position(|(n, _)| n == field)
                    .ok_or_else(|| {
                        format!(
                            "SyntaxError(ProcedureOutputNotFound): procedure `{}` has no output \
                             field `{}`",
                            name, field
                        )
                    })?;
                let var = alias.clone().unwrap_or_else(|| field.clone());
                if already_bound.contains(&var) || !seen.insert(var.clone()) {
                    return Err(format!(
                        "SyntaxError(VariableAlreadyBound): variable `{}` already bound",
                        var
                    ));
                }
                indices.push(idx);
                vars.push(var);
            }
            (indices, vars)
        } else {
            // No YIELD: bind every output field under its own name.
            (
                (0..proc.outputs.len()).collect(),
                proc.outputs.iter().map(|(n, _)| n.clone()).collect(),
            )
        };

        if output_vars.is_empty() {
            // Void procedure: produce a single empty row so an in-query call is an
            // identity over incoming rows. A standalone void call relies on its
            // empty RETURN to produce zero result records.
            return Ok(ResolvedCall {
                output_vars,
                rows: vec![vec![]],
            });
        }

        let rows: Vec<Vec<Value>> = matched_outputs
            .iter()
            .map(|out| selected_indices.iter().map(|&i| out[i].clone()).collect())
            .collect();

        Ok(ResolvedCall { output_vars, rows })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proc_my() -> Procedure {
        Procedure {
            name: "test.my.proc".to_string(),
            inputs: vec![
                ("name".to_string(), CypherType::String),
                ("id".to_string(), CypherType::Integer),
            ],
            outputs: vec![
                ("city".to_string(), CypherType::String),
                ("country_code".to_string(), CypherType::Integer),
            ],
            rows: vec![
                vec![json!("Stefan"), json!(1), json!("Berlin"), json!(49)],
                vec![json!("Stefan"), json!(2), json!("München"), json!(49)],
                vec![json!("Petra"), json!(1), json!("London"), json!(44)],
            ],
        }
    }

    fn registry() -> ProcedureRegistry {
        let mut r = ProcedureRegistry::new();
        r.register(proc_my());
        r
    }

    #[test]
    fn filters_rows_by_arguments() {
        let r = registry();
        let resolved = r
            .resolve(
                "test.my.proc",
                &[json!("Stefan"), json!(1)],
                false,
                true,
                &Some(vec![
                    ("city".to_string(), None),
                    ("country_code".to_string(), None),
                ]),
                false,
                &HashSet::new(),
                &HashMap::new(),
            )
            .unwrap();
        assert_eq!(resolved.output_vars, vec!["city", "country_code"]);
        assert_eq!(resolved.rows, vec![vec![json!("Berlin"), json!(49)]]);
    }

    #[test]
    fn wrong_argument_type_is_rejected() {
        let r = registry();
        let err = r
            .resolve(
                "test.my.proc",
                &[json!(true), json!(1)],
                false,
                false,
                &None,
                false,
                &HashSet::new(),
                &HashMap::new(),
            )
            .unwrap_err();
        assert!(err.contains("InvalidArgumentType"), "{err}");
    }

    #[test]
    fn yield_star_in_query_is_rejected() {
        let r = registry();
        let err = r
            .resolve(
                "test.my.proc",
                &[json!("Stefan"), json!(1)],
                false,
                true,
                &None,
                true,
                &HashSet::new(),
                &HashMap::new(),
            )
            .unwrap_err();
        assert!(err.contains("UnexpectedSyntax"), "{err}");
    }

    #[test]
    fn duplicate_yield_alias_is_rejected() {
        let r = registry();
        let err = r
            .resolve(
                "test.my.proc",
                &[json!("Stefan"), json!(1)],
                false,
                true,
                &Some(vec![
                    ("city".to_string(), Some("c".to_string())),
                    ("country_code".to_string(), Some("c".to_string())),
                ]),
                false,
                &HashSet::new(),
                &HashMap::new(),
            )
            .unwrap_err();
        assert!(err.contains("VariableAlreadyBound"), "{err}");
    }

    #[test]
    fn implicit_args_in_query_with_inputs_is_rejected() {
        let r = registry();
        let err = r
            .resolve(
                "test.my.proc",
                &[],
                true,
                true,
                &Some(vec![("city".to_string(), None)]),
                false,
                &HashSet::new(),
                &HashMap::new(),
            )
            .unwrap_err();
        assert!(err.contains("InvalidArgumentPassingMode"), "{err}");
    }
}
