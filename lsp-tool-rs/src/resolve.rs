//! Symbol → position resolution: the impedance-transformer layer.
//!
//! The caller names a symbol; the LSP wants a zero-indexed UTF-16 position.
//! Resolution is three stages, each leaning on the server for the semantic
//! part so the lexical scan only has to propose, never decide:
//!
//!   1. scan the file for word-boundary occurrences of the identifier;
//!   2. verify each with `prepareRename` — strings, comments, and keywords
//!      come back null and drop out;
//!   3. if several survive, ask for `references` from the first: occurrences
//!      of the *same* symbol are exactly its reference set, so anything not
//!      covered is a genuinely different symbol (shadowing) → ambiguous,
//!      reported with candidate positions so the caller can pass `line:char`.

use anyhow::Result;
use serde_json::{json, Value};

use crate::client::Client;
use crate::ToolError;

/// A CLI target: either a symbol name or an explicit `line:char` position.
pub enum Target {
    Symbol(String),
    Position { line: usize, character: usize },
}

/// `"5:10"` parses as a position; anything else is a symbol name (a legal
/// identifier can't contain `:`).
pub fn parse_target(s: &str) -> Target {
    if let Some((l, c)) = s.split_once(':') {
        if let (Ok(line), Ok(character)) = (l.parse(), c.parse()) {
            return Target::Position { line, character };
        }
    }
    Target::Symbol(s.to_string())
}

#[derive(Clone)]
pub struct Candidate {
    pub line: usize,
    /// UTF-16 column of the occurrence's first character.
    pub character: usize,
    pub line_text: String,
}

impl Candidate {
    fn to_json(&self) -> Value {
        json!({"line": self.line, "character": self.character, "text": self.line_text})
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Word-boundary occurrences of `symbol` in `text`, with UTF-16 columns.
/// Purely lexical — over-matches strings and comments by design; stage 2 filters.
pub fn lexical_candidates(text: &str, symbol: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    if symbol.is_empty() {
        return out;
    }
    for (line_no, line) in text.lines().enumerate() {
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let mut utf16_col = 0usize;
        for (i, &(byte_off, ch)) in chars.iter().enumerate() {
            if line[byte_off..].starts_with(symbol) {
                let prev_ok = i == 0 || !is_word(chars[i - 1].1);
                let after = line[byte_off + symbol.len()..].chars().next();
                let next_ok = after.map_or(true, |c| !is_word(c));
                if prev_ok && next_ok {
                    out.push(Candidate {
                        line: line_no,
                        character: utf16_col,
                        line_text: line.trim_end().to_string(),
                    });
                }
            }
            utf16_col += ch.len_utf16();
        }
    }
    out
}

/// Resolve `symbol` in the opened document to one (line, character), or fail
/// with a structured, actionable error.
pub fn resolve_symbol(
    client: &mut Client,
    uri: &str,
    text: &str,
    file: &str,
    symbol: &str,
) -> Result<(usize, usize)> {
    let candidates = lexical_candidates(text, symbol);
    if candidates.is_empty() {
        return Err(ToolError::new(format!("symbol {symbol:?} not found in {file}")).into());
    }

    let mut verified: Vec<Candidate> = Vec::new();
    for c in &candidates {
        if client.prepare_rename(uri, c.line, c.character)?.is_some() {
            verified.push(c.clone());
        }
    }

    if verified.is_empty() {
        let data = json!({"occurrences": candidates.iter().map(Candidate::to_json).collect::<Vec<_>>()});
        return Err(ToolError::with_data(
            format!(
                "{} occurrence(s) of {symbol:?} in {file}, but none is a renameable symbol \
                 (strings/comments?)",
                candidates.len()
            ),
            data,
        )
        .into());
    }

    if verified.len() == 1 {
        let c = &verified[0];
        return Ok((c.line, c.character));
    }

    // Several verified occurrences. Same symbol ⇔ same reference set: pull
    // references from the first and check the rest are covered.
    let first = verified[0].clone();
    let all_same = match client.references(uri, first.line, first.character) {
        Ok(locs) => verified[1..].iter().all(|c| covered(&locs, uri, c)),
        Err(_) => false, // can't disambiguate without references support
    };
    if all_same {
        return Ok((first.line, first.character));
    }

    let data = json!({
        "candidates": verified.iter().map(Candidate::to_json).collect::<Vec<_>>(),
        "hint": "re-run with an explicit position target: <line>:<character> (zero-indexed)",
    });
    Err(ToolError::with_data(
        format!(
            "{symbol:?} is ambiguous in {file}: {} distinct occurrences verified as symbols; \
             disambiguate with line:char",
            verified.len()
        ),
        data,
    )
    .into())
}

/// Is the candidate's position inside one of the reference ranges in `uri`?
fn covered(locations: &[Value], uri: &str, c: &Candidate) -> bool {
    locations.iter().any(|loc| {
        loc["uri"].as_str() == Some(uri)
            && loc["range"]["start"]["line"].as_u64() == Some(c.line as u64)
            && loc["range"]["start"]["character"].as_u64().map_or(false, |s| s as usize <= c.character)
            && loc["range"]["end"]["character"].as_u64().map_or(false, |e| c.character < e as usize)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_position_vs_symbol() {
        assert!(matches!(parse_target("5:10"), Target::Position { line: 5, character: 10 }));
        assert!(matches!(parse_target("greet"), Target::Symbol(_)));
        // a malformed position spec falls back to symbol, then fails lookup
        assert!(matches!(parse_target("5:x"), Target::Symbol(_)));
    }

    #[test]
    fn word_boundaries_reject_substrings() {
        let text = "greeting = greet(greeter)\ngreet # greet";
        let hits = lexical_candidates(text, "greet");
        let positions: Vec<(usize, usize)> = hits.iter().map(|c| (c.line, c.character)).collect();
        // not `greeting`/`greeter`; lexical scan *does* include the comment —
        // prepareRename filters that later.
        assert_eq!(positions, vec![(0, 11), (1, 0), (1, 8)]);
    }

    #[test]
    fn underscore_is_a_word_char() {
        assert!(lexical_candidates("my_greet = 1", "greet").is_empty());
        assert_eq!(lexical_candidates("_x = _x + 1", "_x").len(), 2);
    }

    #[test]
    fn columns_are_utf16() {
        // 🦀 is one code point but two UTF-16 units.
        let hits = lexical_candidates("s = \"🦀\"; greet()", "greet");
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].line, hits[0].character), (0, 10));
    }
}
