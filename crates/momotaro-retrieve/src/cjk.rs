//! CJK bigram tokenizer for tantivy.
//!
//! Splits text into maximal CJK runs and non-CJK runs. CJK runs of length
//! two or more emit overlapping bigrams; isolated single CJK characters
//! emit a unigram so single-character queries can still hit. Non-CJK runs
//! split on non-alphanumeric boundaries, are lowercased, and drop tokens
//! longer than 40 characters. Offsets are byte offsets into the original
//! input.

use momotaro_contracts::is_cjk;
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer};

/// Name under which [`cjk_analyzer`] is registered on an index.
pub const CJK_TOKENIZER_NAME: &str = "cjk";

/// Longest accepted non-CJK token, in characters; longer tokens are dropped.
const MAX_TOKEN_CHARS: usize = 40;

/// The Momotaro CJK bigram tokenizer.
#[derive(Clone, Default)]
pub struct CjkTokenizer;

impl Tokenizer for CjkTokenizer {
    type TokenStream<'a> = CjkTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        CjkTokenStream {
            tokens: tokenize(text),
            cursor: 0,
        }
    }
}

/// Token stream over a precomputed token list.
pub struct CjkTokenStream {
    tokens: Vec<Token>,
    cursor: usize,
}

impl TokenStream for CjkTokenStream {
    fn advance(&mut self) -> bool {
        if self.cursor < self.tokens.len() {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        &self.tokens[self.cursor - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.cursor - 1]
    }
}

/// Builds the analyzer registered as `"cjk"`. Lowercasing is built in.
pub fn cjk_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(CjkTokenizer).build()
}

fn emit(tokens: &mut Vec<Token>, position: &mut usize, text: String, from: usize, to: usize) {
    tokens.push(Token {
        offset_from: from,
        offset_to: to,
        position: *position,
        position_length: 1,
        text,
    });
    *position += 1;
}

/// Emits a CJK run: overlapping bigrams, or a single unigram for length 1.
fn flush_cjk(run: &[(usize, char)], run_end: usize, tokens: &mut Vec<Token>, position: &mut usize) {
    if run.len() == 1 {
        let (from, c) = run[0];
        emit(tokens, position, c.to_string(), from, from + c.len_utf8());
        return;
    }
    for k in 0..run.len().saturating_sub(1) {
        let (from, c0) = run[k];
        let (_, c1) = run[k + 1];
        let to = run.get(k + 2).map_or(run_end, |&(offset, _)| offset);
        let mut text = String::with_capacity(c0.len_utf8() + c1.len_utf8());
        text.push(c0);
        text.push(c1);
        emit(tokens, position, text, from, to);
    }
}

/// Emits one lowercased word token; overlong tokens are dropped but still
/// consume a position slot, matching tantivy filter-pipeline semantics.
fn flush_word(
    run: &[(usize, char)],
    run_end: usize,
    tokens: &mut Vec<Token>,
    position: &mut usize,
) {
    if run.is_empty() {
        return;
    }
    let text: String = run.iter().flat_map(|&(_, c)| c.to_lowercase()).collect();
    if text.chars().count() > MAX_TOKEN_CHARS {
        *position += 1;
        return;
    }
    let from = run[0].0;
    emit(tokens, position, text, from, run_end);
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut position = 0usize;
    let mut run: Vec<(usize, char)> = Vec::new();
    // None = no open run; Some(true) = CJK run; Some(false) = word run.
    let mut run_kind: Option<bool> = None;

    for (offset, c) in text.char_indices() {
        if is_cjk(c) {
            if run_kind == Some(false) {
                flush_word(&run, offset, &mut tokens, &mut position);
                run.clear();
            }
            run.push((offset, c));
            run_kind = Some(true);
        } else if c.is_alphanumeric() {
            if run_kind == Some(true) {
                flush_cjk(&run, offset, &mut tokens, &mut position);
                run.clear();
            }
            run.push((offset, c));
            run_kind = Some(false);
        } else {
            match run_kind {
                Some(true) => flush_cjk(&run, offset, &mut tokens, &mut position),
                Some(false) => flush_word(&run, offset, &mut tokens, &mut position),
                None => {}
            }
            run.clear();
            run_kind = None;
        }
    }
    match run_kind {
        Some(true) => flush_cjk(&run, text.len(), &mut tokens, &mut position),
        Some(false) => flush_word(&run, text.len(), &mut tokens, &mut position),
        None => {}
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens_of(text: &str) -> Vec<Token> {
        let mut analyzer = cjk_analyzer();
        let mut stream = analyzer.token_stream(text);
        let mut tokens = Vec::new();
        stream.process(&mut |token| tokens.push(token.clone()));
        tokens
    }

    fn texts(tokens: &[Token]) -> Vec<&str> {
        tokens.iter().map(|t| t.text.as_str()).collect()
    }

    #[test]
    fn cjk_run_becomes_overlapping_bigrams() {
        let tokens = tokens_of("傅里叶变换");
        assert_eq!(texts(&tokens), ["傅里", "里叶", "叶变", "变换"]);
        for (i, token) in tokens.iter().enumerate() {
            assert_eq!(token.position, i);
            assert_eq!(token.position_length, 1);
        }
        assert_eq!(tokens[0].offset_from, 0);
        assert_eq!(tokens[0].offset_to, 6);
        assert_eq!(tokens[3].offset_from, 9);
        assert_eq!(tokens[3].offset_to, 15);
    }

    #[test]
    fn isolated_cjk_char_is_unigram() {
        let tokens = tokens_of("中");
        assert_eq!(texts(&tokens), ["中"]);
        assert_eq!(tokens[0].position, 0);
        assert_eq!(tokens[0].offset_from, 0);
        assert_eq!(tokens[0].offset_to, 3);
    }

    #[test]
    fn mixed_latin_and_cjk() {
        let tokens = tokens_of("BERT模型");
        assert_eq!(texts(&tokens), ["bert", "模型"]);
        assert_eq!(tokens[0].position, 0);
        assert_eq!(tokens[1].position, 1);
        assert_eq!(tokens[1].offset_from, 4);
        assert_eq!(tokens[1].offset_to, 10);
    }

    #[test]
    fn latin_words_split_on_punctuation_and_lowercase() {
        let tokens = tokens_of("hello, world");
        assert_eq!(texts(&tokens), ["hello", "world"]);
        assert_eq!(tokens[1].offset_from, 7);
        assert_eq!(tokens[1].offset_to, 12);
    }

    #[test]
    fn overlong_latin_token_is_dropped() {
        let dropped = tokens_of(&format!("{} tail", "a".repeat(50)));
        assert_eq!(texts(&dropped), ["tail"]);
        // 40 chars is exactly at the limit and is kept.
        let kept = tokens_of(&"a".repeat(40));
        assert_eq!(texts(&kept).len(), 1);
    }

    #[test]
    fn offsets_are_valid_byte_ranges() {
        let text = "Hi你好世界 ok!";
        for token in tokens_of(text) {
            assert!(token.offset_from <= token.offset_to);
            assert!(token.offset_to <= text.len());
            assert!(text.get(token.offset_from..token.offset_to).is_some());
            assert!(!token.text.is_empty());
        }
    }

    #[test]
    fn empty_input_produces_no_tokens() {
        assert!(tokens_of("").is_empty());
        assert!(tokens_of(" !?. ").is_empty());
    }
}
