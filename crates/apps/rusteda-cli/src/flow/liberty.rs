// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! The bounded Liberty group reader of the recipe's delay and drive profiles
//! (`liberty_lite.py`): library units, `lu_table_template` groups and selected `cell` groups as
//! nested groups, located by a top-level scan so that a large library is not tokenized as a
//! whole, and the bilinear table lookup without extrapolation. The token and number rules are
//! the reference's, so every value read here is the value it reads.

use std::path::Path;

use rustc_hash::FxHashMap as HashMap;

use super::context::{Failure, Res};
use crate::estimate::space;

const MAX_LIBRARY_BYTES: usize = 128 * 1024 * 1024;
const MAX_GROUP_TOKENS: usize = 2_000_000;

fn error<T>(text: impl Into<String>) -> Res<T> {
    Err(Failure::new("LIBERTY", text))
}

fn punct(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'{' | b'}' | b':' | b';' | b',')
}

/// The tokens of a Liberty text: quoted strings (with their quotes), the punctuation
/// `(){}:;,` and runs of other characters; whitespace, comments and line continuations are
/// dropped.
fn tokens(text: &str) -> Res<Vec<&str>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if space(c) {
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            // `/\*.*?\*/`: to the first `*/` after the opening.
            if let Some(k) = text[i + 2..].find("*/") {
                i += 2 + k + 2;
                continue;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'/') {
            i += text[i..].find('\n').unwrap_or(b.len() - i);
            continue;
        } else if c == b'\\' && b.get(i + 1) == Some(&b'\n') {
            i += 2;
            continue;
        }
        let start = i;
        if c == b'"' {
            i += 1;
            loop {
                match b.get(i) {
                    None => return error(format!("untokenizable Liberty text at offset {start}")),
                    Some(b'"') => break,
                    Some(b'\\') if i + 1 < b.len() => i += 2,
                    Some(b'\\') => {
                        return error(format!("untokenizable Liberty text at offset {start}"));
                    }
                    Some(_) => i += 1,
                }
            }
            i += 1;
        } else if punct(c) {
            i += 1;
        } else {
            while i < b.len() && !space(b[i]) && !punct(b[i]) && b[i] != b'"' {
                i += 1;
            }
        }
        out.push(&text[start..i]);
        if out.len() > MAX_GROUP_TOKENS {
            return error("Liberty group token budget exceeded");
        }
    }
    Ok(out)
}

pub(crate) fn unquote(t: &str) -> &str {
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// `name (args) { attributes; groups }` with ordered children.
#[derive(Debug, Default)]
pub(crate) struct Group {
    pub(crate) kind: String,
    /// The raw argument tokens.
    pub(crate) args: Vec<String>,
    /// Simple attributes in order: the value tokens joined by one space (a repeated key keeps
    /// its first position and takes the last value).
    pub(crate) attrs: Vec<(String, String)>,
    /// Complex attributes in order: every occurrence's unquoted arguments.
    pub(crate) complex: Vec<(String, Vec<Vec<String>>)>,
    pub(crate) groups: Vec<Group>,
}

impl Group {
    pub(crate) fn name(&self) -> &str {
        self.args.first().map_or("", |a| unquote(a))
    }
    /// The child groups of `kind` in order.
    pub(crate) fn sub<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Group> + 'a {
        self.groups.iter().filter(move |g| g.kind == kind)
    }
    /// The unquoted value of a simple attribute.
    pub(crate) fn attr(&self, key: &str) -> Option<&str> {
        self.raw(key).map(unquote)
    }
    /// The value of a simple attribute as the tokens wrote it.
    pub(crate) fn raw(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    pub(crate) fn complex(&self, key: &str) -> Option<&Vec<Vec<String>>> {
        self.complex.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

fn tok<'a>(toks: &[&'a str], i: usize) -> Res<&'a str> {
    toks.get(i)
        .copied()
        .ok_or_else(|| Failure::new("LIBERTY", "list index out of range"))
}

fn parse_group(toks: &[&str], i: usize) -> Res<(Group, usize)> {
    let kind = tok(toks, i)?;
    if tok(toks, i + 1)? != "(" {
        return error(format!("group {kind} without arguments"));
    }
    let mut j = i + 2;
    let mut args = Vec::new();
    while tok(toks, j)? != ")" {
        if toks[j] != "," {
            args.push(toks[j].to_string());
        }
        j += 1;
    }
    if tok(toks, j + 1)? != "{" {
        return error(format!("group {kind} without body"));
    }
    let mut g = Group {
        kind: kind.to_string(),
        args,
        ..Group::default()
    };
    j += 2;
    while tok(toks, j)? != "}" {
        let key = toks[j];
        let next = tok(toks, j + 1)?;
        if next == ":" {
            let mut k = j + 2;
            let mut value: Vec<&str> = Vec::new();
            while tok(toks, k)? != ";" {
                value.push(toks[k]);
                k += 1;
                if matches!(tok(toks, k)?, "}" | "{") {
                    return error(format!("unterminated attribute {key}"));
                }
            }
            let value = value.join(" ");
            match g.attrs.iter_mut().find(|(name, _)| name == key) {
                Some(slot) => slot.1 = value,
                None => g.attrs.push((key.to_string(), value)),
            }
            j = k + 1;
        } else if next == "(" {
            let mut k = j + 2;
            let mut cargs = Vec::new();
            while tok(toks, k)? != ")" {
                if toks[k] != "," {
                    cargs.push(unquote(toks[k]).to_string());
                }
                k += 1;
            }
            // The reference reads `toks[k + 1]` here: a complex attribute that ends the text
            // is an error there too.
            if tok(toks, k + 1)? == "{" {
                let (child, after) = parse_group(toks, j)?;
                g.groups.push(child);
                j = after;
            } else {
                match g.complex.iter_mut().find(|(name, _)| name == key) {
                    Some(slot) => slot.1.push(cargs),
                    None => g.complex.push((key.to_string(), vec![cargs])),
                }
                j = k + 1;
                if toks.get(j) == Some(&";") {
                    j += 1;
                }
            }
        } else {
            return error(format!("unexpected Liberty token '{key}' in {kind}"));
        }
    }
    Ok((g, j + 1))
}

/// The end (exclusive) of the `{...}` block that opens at `open_brace`; strings and block
/// comments do not count.
fn block_end(text: &str, open_brace: usize) -> Res<usize> {
    let b = text.as_bytes();
    let mut depth = 0i64;
    let mut i = open_brace;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            match text[i + 1..].find('"') {
                Some(k) => i += 1 + k + 1,
                None => return error("substring not found"),
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            match text[i + 2..].find("*/") {
                Some(k) => i += 2 + k + 2,
                None => return error("substring not found"),
            }
            continue;
        }
        if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                return Ok(i + 1);
            }
        }
        i += 1;
    }
    error("unbalanced Liberty braces")
}

/// A match of `^[ \t]*cell\s*\(\s*"?([A-Za-z0-9_$]+)"?\s*\)\s*\{` at the line start `at`:
/// the name and the end of the match.
fn cell_at(text: &str, at: usize) -> Option<(&str, usize)> {
    let b = text.as_bytes();
    let mut i = at;
    while matches!(b.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    if !b[i..].starts_with(b"cell") {
        return None;
    }
    i += 4;
    let skip = |mut i: usize| {
        while b.get(i).is_some_and(|&c| space(c)) {
            i += 1;
        }
        i
    };
    i = skip(i);
    if b.get(i) != Some(&b'(') {
        return None;
    }
    i = skip(i + 1);
    if b.get(i) == Some(&b'"') {
        i += 1;
    }
    let start = i;
    while b
        .get(i)
        .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
    {
        i += 1;
    }
    if i == start {
        return None;
    }
    let name = &text[start..i];
    if b.get(i) == Some(&b'"') {
        i += 1;
    }
    i = skip(i);
    if b.get(i) != Some(&b')') {
        return None;
    }
    i = skip(i + 1);
    (b.get(i) == Some(&b'{')).then_some((name, i + 1))
}

/// The header and the wanted cells of one Liberty file.
#[derive(Debug)]
pub(crate) struct Library {
    pub(crate) header: Group,
    /// The wanted cells in the order of the file (a repeated name keeps its first position and
    /// takes the last group).
    pub(crate) cells: Vec<(String, Group)>,
    templates: HashMap<String, usize>,
}

impl Library {
    /// Reads `path`; with `wanted`, only those cells are parsed.
    pub(crate) fn read(path: &Path, wanted: Option<&[&str]>) -> Res<Library> {
        let data = std::fs::read(path)?;
        Library::parse(&data, wanted)
    }

    pub(crate) fn parse(data: &[u8], wanted: Option<&[&str]>) -> Res<Library> {
        if data.len() > MAX_LIBRARY_BYTES {
            return error("Liberty file exceeds byte budget");
        }
        if !data.is_ascii() {
            return error("'ascii' codec can't decode the Liberty file");
        }
        let text = std::str::from_utf8(data).map_err(|e| Failure::new("LIBERTY", e.to_string()))?;
        // The matches of the cell pattern, scanned as `finditer` does: from the end of the
        // previous match, at line starts only.
        let mut found: Vec<(&str, usize, usize)> = Vec::new();
        let mut at = 0usize;
        loop {
            if let Some((name, end)) = cell_at(text, at) {
                found.push((name, at, end));
                // The next match starts at a line start at or after the end of this one.
                match text[end - 1..].find('\n') {
                    Some(k) => at = end - 1 + k + 1,
                    None => break,
                }
                continue;
            }
            match text[at..].find('\n') {
                Some(k) => at += k + 1,
                None => break,
            }
        }
        let Some(first) = found.first() else {
            return error("no cell groups");
        };
        let header_text = format!("{}}}", &text[..first.1]);
        let (header, _) = parse_group(&tokens(&header_text)?, 0)?;
        let mut cells: Vec<(String, Group)> = Vec::new();
        for &(name, start, end) in &found {
            if wanted.is_some_and(|w| !w.contains(&name)) {
                continue;
            }
            let stop = block_end(text, end - 1)?;
            let (g, _) = parse_group(&tokens(&text[start..stop])?, 0)?;
            match cells.iter_mut().find(|(n, _)| n == name) {
                Some(slot) => slot.1 = g,
                None => cells.push((name.to_string(), g)),
            }
        }
        let mut templates = HashMap::default();
        for (k, g) in header.groups.iter().enumerate() {
            if g.kind == "lu_table_template" {
                templates.insert(g.name().to_string(), k);
            }
        }
        Ok(Library {
            header,
            cells,
            templates,
        })
    }

    pub(crate) fn cell(&self, name: &str) -> Option<&Group> {
        self.cells.iter().find(|(n, _)| n == name).map(|(_, g)| g)
    }

    fn template(&self, name: &str) -> Option<&Group> {
        self.templates.get(name).map(|&k| &self.header.groups[k])
    }

    pub(crate) fn time_unit_ns(&self) -> Res<f64> {
        match self.header.attr("time_unit") {
            Some("1ns") => Ok(1.0),
            Some("1ps") => Ok(1e-3),
            Some("10ps") => Ok(1e-2),
            Some("100ps") => Ok(0.1),
            other => error(format!("unsupported time_unit {}", other.unwrap_or("None"))),
        }
    }

    pub(crate) fn cap_unit_pf(&self) -> Res<f64> {
        let Some(c) = self
            .header
            .complex("capacitive_load_unit")
            .and_then(|c| c.first())
            .filter(|c| c.len() == 2)
        else {
            return error("capacitive_load_unit missing");
        };
        let scale = float(&c[0])?;
        match c[1].to_ascii_lowercase().as_str() {
            "pf" => Ok(scale * 1.0),
            "ff" => Ok(scale * 1e-3),
            unit => error(format!("'{unit}'")),
        }
    }
}

/// `float(text)` of a Liberty number.
pub(crate) fn float(t: &str) -> Res<f64> {
    let s = t.trim_matches(|c: char| c.is_ascii() && space(c as u8));
    let ok = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'));
    ok.then(|| s.parse::<f64>().ok()).flatten().ok_or_else(|| {
        Failure::new(
            "LIBERTY",
            format!("could not convert string to float: '{t}'"),
        )
    })
}

/// The numbers of a Liberty list: backslashes and quotes read as blanks, items split at commas.
fn numbers(s: &str) -> Res<Vec<f64>> {
    let cleaned = s.replace(['\\', '"'], " ");
    let blank = |x: &str| x.bytes().all(space);
    cleaned
        .split(',')
        .filter(|x| !blank(x))
        .map(float)
        .collect()
}

/// An NLDM 2-D table with axes named by the template variables.
#[derive(Debug)]
pub(crate) struct Table {
    pub(crate) variables: [Option<String>; 2],
    pub(crate) index_1: Vec<f64>,
    pub(crate) index_2: Vec<f64>,
    pub(crate) values: Vec<Vec<f64>>,
}

impl Table {
    pub(crate) fn new(group: &Group, library: &Library) -> Res<Table> {
        let Some(template) = library.template(group.name()) else {
            return error(format!("table template {} missing", group.name()));
        };
        let variables = [
            template.attr("variable_1").map(str::to_string),
            template.attr("variable_2").map(str::to_string),
        ];
        let idx = |k: &str| -> Res<Option<Vec<f64>>> {
            let source = group.complex(k).or_else(|| template.complex(k));
            match source.and_then(|c| c.first()) {
                Some(args) => Ok(Some(numbers(&args.join(","))?)),
                None => Ok(None),
            }
        };
        let (index_1, index_2) = (idx("index_1")?, idx("index_2")?);
        let Some(rows) = group.complex("values").and_then(|c| c.first()) else {
            return error("'values'");
        };
        let values = rows.iter().map(|r| numbers(r)).collect::<Res<Vec<_>>>()?;
        let Some(index_2) = index_2 else {
            return error("only 2-D tables are supported");
        };
        let Some(index_1) = index_1 else {
            return error("object of type 'NoneType' has no len()");
        };
        if values.len() != index_1.len() || values.iter().any(|r| r.len() != index_2.len()) {
            return error("table shape differs from its indices");
        }
        Ok(Table {
            variables,
            index_1,
            index_2,
            values,
        })
    }

    /// Whether the axes are (`input_net_transition`, `total_output_net_capacitance`).
    pub(crate) fn slew_by_load(&self) -> bool {
        self.variables[0].as_deref() == Some("input_net_transition")
            && self.variables[1].as_deref() == Some("total_output_net_capacitance")
    }

    pub(crate) fn lookup(&self, slew: f64, load: f64) -> Res<f64> {
        let pick = |v: &Option<String>| match v.as_deref() {
            Some("input_net_transition") => Some(slew),
            Some("total_output_net_capacitance") => Some(load),
            _ => None,
        };
        let (Some(x), Some(y)) = (pick(&self.variables[0]), pick(&self.variables[1])) else {
            return error("unsupported table variables");
        };
        if self.variables[0] == self.variables[1] {
            return error("unsupported table variables");
        }
        bilinear(&self.index_1, &self.index_2, &self.values, x, y)
    }
}

/// `bisect.bisect_right(axis, v)`.
fn bisect_right(axis: &[f64], v: f64) -> usize {
    let (mut lo, mut hi) = (0, axis.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        if v < axis[mid] {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

fn bilinear(xs: &[f64], ys: &[f64], values: &[Vec<f64>], x: f64, y: f64) -> Res<f64> {
    if xs.len() < 2 || ys.len() < 2 {
        return error("table axis shorter than two points");
    }
    if !(xs[0] <= x && x <= xs[xs.len() - 1] && ys[0] <= y && y <= ys[ys.len() - 1]) {
        return error(format!(
            "point ({}, {}) outside the characterized table; no extrapolation",
            crate::estimate::repr(x),
            crate::estimate::repr(y)
        ));
    }
    let bracket = |axis: &[f64], v: f64| {
        let k = (bisect_right(axis, v).max(1) - 1).min(axis.len() - 2);
        let t = (v - axis[k]) / (axis[k + 1] - axis[k]);
        (k, t)
    };
    let (i, tx) = bracket(xs, x);
    let (j, ty) = bracket(ys, y);
    let a = values[i][j] * (1.0 - ty) + values[i][j + 1] * ty;
    let b = values[i + 1][j] * (1.0 - ty) + values[i + 1][j + 1] * ty;
    Ok(a * (1.0 - tx) + b * tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIB: &str = r#"library (demo) {
  /* units */
  time_unit : "1ns" ;
  capacitive_load_unit (1, pf) ;
  lu_table_template (T2) {
    variable_1 : input_net_transition ;
    variable_2 : total_output_net_capacitance ;
    index_1 ("0.1, 0.5") ;
    index_2 ("0.01, 0.02, 0.04") ;
  }
  cell (BUF) {
    area : 2.5 ;
    pin (A) { direction : input ; capacitance : 0.002 ; }
    pin (Y) {
      direction : output ; function : "(A)" ;
      timing () {
        related_pin : "A" ; timing_sense : positive_unate ;
        cell_rise (T2) {
          values ("0.10, 0.20, 0.40", \
                  "0.30, 0.50, 0.90") ;
        }
      }
    }
  }
  cell ("OTHER") { area : 1 ; }
}
"#;

    #[test]
    fn reads_units_cells_and_tables_like_the_reference() {
        let lib = Library::parse(LIB.as_bytes(), Some(&["BUF"])).unwrap();
        assert_eq!(lib.cells.len(), 1);
        assert_eq!(lib.time_unit_ns().unwrap(), 1.0);
        assert_eq!(lib.cap_unit_pf().unwrap(), 1.0);
        let cell = lib.cell("BUF").unwrap();
        assert_eq!(cell.attr("area"), Some("2.5"));
        let pins: Vec<&Group> = cell.sub("pin").collect();
        assert_eq!(pins[1].attr("function"), Some("(A)"));
        let arc = pins[1].sub("timing").next().unwrap();
        let table = Table::new(arc.sub("cell_rise").next().unwrap(), &lib).unwrap();
        assert!(table.slew_by_load());
        assert_eq!(table.values, [[0.10, 0.20, 0.40], [0.30, 0.50, 0.90]]);
        // The reference (liberty_lite.py) reads the same values: Table.lookup(0.2, 0.03) is
        // 0.39999999999999997 and Table.lookup(0.5, 0.04) is 0.9.
        assert_eq!(table.lookup(0.2, 0.03).unwrap(), 0.39999999999999997);
        assert_eq!(table.lookup(0.5, 0.04).unwrap(), 0.9);
        assert!(table.lookup(0.6, 0.03).is_err());
    }
}
