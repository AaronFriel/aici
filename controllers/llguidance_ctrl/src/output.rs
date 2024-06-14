use aici_abi::bytes::to_hex_string;
use serde::{Deserialize, Serialize};

use crate::{earley, TokenParser};

#[derive(Serialize, Deserialize)]
pub struct Capture {
    object: &'static str, // "capture"
    name: String,
    str: String,
    hex: String,
    log_prob: f64,
}

#[derive(Serialize, Deserialize)]
pub struct FinalText {
    object: &'static str, // "final_text"
    str: String,
    hex: String,
}

#[derive(Serialize, Deserialize)]
pub struct Text {
    object: &'static str, // "text"
    str: String,
    hex: String,
    log_prob: f64,
    num_tokens: usize,
}

#[derive(Serialize, Deserialize)]
pub struct Stats {
    object: &'static str, // "stats"
    #[serde(flatten)]
    stats: earley::ParserStats,
}

impl Text {
    pub fn from_bytes(bytes: &[u8], log_prob: f64, num_tokens: usize) -> Self {
        Text {
            object: "text",
            str: String::from_utf8_lossy(bytes).to_string(),
            hex: to_hex_string(bytes),
            log_prob,
            num_tokens,
        }
    }
}

impl FinalText {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        FinalText {
            object: "final_text",
            str: String::from_utf8_lossy(bytes).to_string(),
            hex: to_hex_string(bytes),
        }
    }
}

pub struct Reporter {
    reported_captures: usize,
    text_ptr: usize,
    token_ptr: usize,
    prev_stats: earley::ParserStats,
}

impl Reporter {
    pub fn new(tok_parser: &TokenParser) -> Self {
        Reporter {
            reported_captures: 0,
            text_ptr: 0,
            token_ptr: tok_parser.num_tokens(),
            prev_stats: tok_parser.parser.stats().clone(),
        }
    }

    pub fn get_progress(
        &mut self,
        tok_parser: &mut TokenParser,
        is_final: bool,
    ) -> Vec<serde_json::Value> {
        let mut res = vec![];
        // first report newly generated text
        let num_tokens = tok_parser.num_tokens();
        let new_text = tok_parser.bytes_since(self.text_ptr);
        if new_text.len() > 0 {
            // TODO log_prob
            let text = Text::from_bytes(new_text, 0.0, num_tokens - self.token_ptr);
            res.push(serde_json::to_value(&text).unwrap());
            self.text_ptr += new_text.len();
            self.token_ptr = num_tokens;
        }

        // then the captures
        let captures = &tok_parser.parser.captures()[self.reported_captures..];
        self.reported_captures += captures.len();

        // remove duplicate names
        let mut seen = std::collections::HashSet::new();
        let captures = captures
            .iter()
            .rev()
            .filter(|(name, _)| seen.insert(name))
            .collect::<Vec<_>>();
        for (name, val) in captures.iter().rev() {
            let cap = Capture {
                object: "capture",
                name: name.clone(),
                str: String::from_utf8_lossy(val).to_string(),
                hex: to_hex_string(val),
                log_prob: 0.0, // TODO
            };
            res.push(serde_json::to_value(&cap).unwrap());
        }

        if is_final {
            let final_text = FinalText::from_bytes(tok_parser.final_bytes());
            res.push(serde_json::to_value(&final_text).unwrap());
        }

        let delta = tok_parser.parser.stats().delta(&self.prev_stats);
        self.prev_stats = tok_parser.parser.stats().clone();
        res.push(
            serde_json::to_value(&Stats {
                object: "stats",
                stats: delta,
            })
            .unwrap(),
        );

        res
    }
}