//! A mini parser for a TOML subset used by cell manifests (no external dependencies).
//! Supports: [section], key = "str" | int | true/false | [int, int].
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum Val {
    Str(String),
    Int(i64),
    Bool(bool),
    Ints(Vec<i64>),
}

pub struct Manifest {
    m: HashMap<String, Val>,
}

impl Manifest {
    pub fn load(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let mut m = HashMap::new();
        let mut section = String::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap().trim();
            if line.is_empty() { continue; }
            if let Some(s) = line.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
                section = s.trim().to_string();
                continue;
            }
            let (k, v) = line.split_once('=').ok_or(format!("line {}: no '='", n + 1))?;
            let key = format!("{}.{}", section, k.trim());
            m.insert(key, parse_val(v.trim()).map_err(|e| format!("line {}: {e}", n + 1))?);
        }
        Ok(Manifest { m })
    }

    pub fn str(&self, k: &str) -> Result<String, String> {
        match self.m.get(k) { Some(Val::Str(s)) => Ok(s.clone()), _ => Err(format!("no string {k}")) }
    }
    pub fn int(&self, k: &str) -> Result<i64, String> {
        match self.m.get(k) { Some(Val::Int(i)) => Ok(*i), _ => Err(format!("no number {k}")) }
    }
    pub fn bool(&self, k: &str) -> bool {
        matches!(self.m.get(k), Some(Val::Bool(true)))
    }
    pub fn ints(&self, k: &str) -> Result<Vec<i64>, String> {
        match self.m.get(k) { Some(Val::Ints(v)) => Ok(v.clone()), _ => Err(format!("no array {k}")) }
    }
}

fn parse_val(s: &str) -> Result<Val, String> {
    if let Some(inner) = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        return Ok(Val::Str(inner.to_string()));
    }
    if s == "true" { return Ok(Val::Bool(true)); }
    if s == "false" { return Ok(Val::Bool(false)); }
    if let Some(inner) = s.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
        let mut v = Vec::new();
        for p in inner.split(',') {
            let p = p.trim();
            if p.is_empty() { continue; }
            v.push(p.parse().map_err(|_| format!("not a number: {p}"))?);
        }
        return Ok(Val::Ints(v));
    }
    s.parse::<i64>().map(Val::Int).map_err(|_| format!("unrecognised value: {s}"))
}
