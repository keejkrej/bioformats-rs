use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AxisType {
    Z,
    C,
    T,
    Series,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePatternBlock {
    raw: String,
    start: i32,
    end: i32,
    step: i32,
    width: usize,
}

impl FilePatternBlock {
    pub fn parse(raw: &str) -> Option<Self> {
        let mut pieces = raw.split(':');
        let range = pieces.next()?;
        let step = pieces
            .next()
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(1)
            .max(1);
        let mut bounds = range.split('-');
        let start_raw = bounds.next()?;
        let end_raw = bounds.next()?;
        let start = start_raw.parse::<i32>().ok()?;
        let end = end_raw.parse::<i32>().ok()?;
        let width = start_raw.len().max(end_raw.len());
        Some(Self {
            raw: raw.to_string(),
            start,
            end,
            step,
            width,
        })
    }

    pub fn len(&self) -> usize {
        if self.end < self.start {
            0
        } else {
            ((self.end - self.start) / self.step + 1) as usize
        }
    }

    pub fn value_at(&self, index: usize) -> String {
        let value = self.start + index as i32 * self.step;
        format!("{:0width$}", value, width = self.width)
    }

    pub fn axis_hint(&self, literal_prefix: &str, literal_suffix: &str) -> AxisType {
        let hint = literal_prefix
            .chars()
            .rev()
            .find(|ch| ch.is_ascii_alphabetic())
            .or_else(|| literal_suffix.chars().find(|ch| ch.is_ascii_alphabetic()))
            .map(|ch| ch.to_ascii_uppercase());
        match hint {
            Some('Z') => AxisType::Z,
            Some('C') => AxisType::C,
            Some('T') => AxisType::T,
            Some('S') => AxisType::Series,
            _ => AxisType::Unknown,
        }
    }
}

impl fmt::Display for FilePatternBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.raw)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePattern {
    pattern: String,
    prefix: String,
    literals: Vec<String>,
    blocks: Vec<FilePatternBlock>,
}

impl FilePattern {
    pub fn parse(pattern: impl Into<String>) -> Option<Self> {
        let pattern = pattern.into();
        let mut prefix = String::new();
        let mut literals = Vec::new();
        let mut blocks = Vec::new();
        let mut cursor = 0usize;

        while let Some(open_rel) = pattern[cursor..].find('<') {
            let open = cursor + open_rel;
            let close = open + pattern[open..].find('>')?;
            let literal = &pattern[cursor..open];
            if blocks.is_empty() {
                prefix.push_str(literal);
            } else {
                literals.push(literal.to_string());
            }
            blocks.push(FilePatternBlock::parse(&pattern[open + 1..close])?);
            cursor = close + 1;
        }

        if blocks.is_empty() {
            return None;
        }

        literals.push(pattern[cursor..].to_string());
        Some(Self {
            pattern,
            prefix,
            literals,
            blocks,
        })
    }

    pub fn new(pattern: impl Into<String>) -> Option<Self> {
        Self::parse(pattern)
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn blocks(&self) -> &[FilePatternBlock] {
        &self.blocks
    }

    pub fn files(&self) -> Vec<PathBuf> {
        let lengths: Vec<usize> = self.blocks.iter().map(FilePatternBlock::len).collect();
        let mut out = Vec::new();
        let mut digits = vec![0usize; self.blocks.len()];
        self.expand_recursive(0, &lengths, &mut digits, &mut out);
        out
    }

    fn expand_recursive(
        &self,
        block_index: usize,
        lengths: &[usize],
        digits: &mut [usize],
        out: &mut Vec<PathBuf>,
    ) {
        if block_index == self.blocks.len() {
            out.push(PathBuf::from(self.expand_with_digits(digits)));
            return;
        }
        for digit in 0..lengths[block_index] {
            digits[block_index] = digit;
            self.expand_recursive(block_index + 1, lengths, digits, out);
        }
    }

    fn expand_with_digits(&self, digits: &[usize]) -> String {
        let mut path = self.prefix.clone();
        for (index, block) in self.blocks.iter().enumerate() {
            path.push_str(&block.value_at(digits[index]));
            path.push_str(&self.literals[index]);
        }
        path
    }

    pub fn find_pattern(path: &Path) -> String {
        let Some(parent) = path.parent() else {
            return path.display().to_string();
        };
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            return path.display().to_string();
        };
        let target = split_numeric_tokens(name);
        let mut candidates = Vec::new();
        let Ok(entries) = std::fs::read_dir(parent) else {
            return path.display().to_string();
        };

        for entry in entries.flatten() {
            let Some(candidate_name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let tokens = split_numeric_tokens(&candidate_name);
            if tokens.len() != target.len() {
                continue;
            }
            let compatible =
                tokens
                    .iter()
                    .zip(target.iter())
                    .all(|(left, right)| match (left, right) {
                        (Token::Text(a), Token::Text(b)) => a == b,
                        (Token::Digits(_), Token::Digits(_)) => true,
                        _ => false,
                    });
            if compatible {
                candidates.push(tokens);
            }
        }

        if candidates.len() <= 1 {
            return path.display().to_string();
        }

        let mut pattern_name = String::new();
        for index in 0..target.len() {
            match &target[index] {
                Token::Text(text) => pattern_name.push_str(text),
                Token::Digits(digits) => {
                    let values: BTreeSet<String> = candidates
                        .iter()
                        .filter_map(|tokens| match &tokens[index] {
                            Token::Digits(value) => Some(value.clone()),
                            _ => None,
                        })
                        .collect();
                    if values.len() <= 1 {
                        pattern_name.push_str(digits);
                        continue;
                    }
                    let start = values
                        .iter()
                        .next()
                        .cloned()
                        .unwrap_or_else(|| digits.clone());
                    let end = values
                        .iter()
                        .next_back()
                        .cloned()
                        .unwrap_or_else(|| digits.clone());
                    pattern_name.push('<');
                    pattern_name.push_str(&format!("{}-{}", start, end));
                    pattern_name.push('>');
                }
            }
        }
        parent.join(pattern_name).display().to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxisGuesser {
    pattern: FilePattern,
    axis_types: Vec<AxisType>,
}

impl AxisGuesser {
    pub fn new(pattern: FilePattern, size_z: u32, size_t: u32, effective_size_c: u32) -> Self {
        let mut axis_types = Vec::with_capacity(pattern.blocks.len());
        let mut available = vec![
            (AxisType::Z, size_z == 1),
            (AxisType::T, size_t == 1),
            (AxisType::C, effective_size_c <= 1),
        ];

        for (index, block) in pattern.blocks.iter().enumerate() {
            let hint = block.axis_hint(
                if index == 0 {
                    &pattern.prefix
                } else {
                    &pattern.literals[index - 1]
                },
                &pattern.literals[index],
            );
            if hint != AxisType::Unknown {
                axis_types.push(hint);
                continue;
            }

            let fallback = available
                .iter_mut()
                .find_map(|(axis, allowed)| {
                    if *allowed {
                        *allowed = false;
                        Some(*axis)
                    } else {
                        None
                    }
                })
                .unwrap_or(AxisType::Unknown);
            axis_types.push(fallback);
        }

        Self {
            pattern,
            axis_types,
        }
    }

    pub fn axis_types(&self) -> &[AxisType] {
        &self.axis_types
    }

    pub fn set_axis_types(&mut self, axis_types: Vec<AxisType>) {
        self.axis_types = axis_types;
    }

    pub fn pattern(&self) -> &FilePattern {
        &self.pattern
    }
}

#[derive(Debug, Clone)]
enum Token {
    Text(String),
    Digits(String),
}

fn split_numeric_tokens(value: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.peek().copied() {
        let digit = ch.is_ascii_digit();
        let mut buf = String::new();
        while let Some(next) = chars.peek().copied() {
            if next.is_ascii_digit() == digit {
                buf.push(next);
                chars.next();
            } else {
                break;
            }
        }
        tokens.push(if digit {
            Token::Digits(buf)
        } else {
            Token::Text(buf)
        });
    }
    tokens
}
