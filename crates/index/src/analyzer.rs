//! Custom text analyzers for MSearchDB.
//!
//! Analyzers control how text is tokenized and normalized before being indexed.
//! MSearchDB provides several built-in analyzers registered on every Tantivy
//! [`Index`]:
//!
//! - **`"standard"`** — lowercase + English stop word removal (default for text fields)
//! - **`"keyword"`** — no tokenization; treats the entire value as a single token
//! - **`"ngram"`** — n-gram tokenizer (min=2, max=3) for partial/substring matching
//! - **`"cjk"`** — Chinese/Japanese/Korean tokenizer powered by `jieba-rs`
//!
//! # Examples
//!
//! ```no_run
//! use msearchdb_index::analyzer::register_analyzers;
//! use tantivy::Index;
//! use tantivy::schema::SchemaBuilder;
//!
//! let schema = SchemaBuilder::default().build();
//! let index = Index::create_in_ram(schema);
//! register_analyzers(&index);
//! ```

use tantivy::tokenizer::{
    LowerCaser, NgramTokenizer, RawTokenizer, RemoveLongFilter, SimpleTokenizer, StopWordFilter,
    TextAnalyzer,
};
use tantivy::Index;

// ---------------------------------------------------------------------------
// Stop word lists
// ---------------------------------------------------------------------------

/// Common English stop words for the standard analyzer.
const ENGLISH_STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with",
];

/// Common Portuguese stop words for the portuguese analyzer.
const PORTUGUESE_STOP_WORDS: &[&str] = &[
    "a", "ao", "aos", "aquela", "aquelas", "aquele", "aqueles", "aquilo", "as", "até", "com",
    "como", "da", "das", "de", "dela", "delas", "dele", "deles", "depois", "do", "dos", "e", "ela",
    "elas", "ele", "eles", "em", "entre", "era", "essa", "essas", "esse", "esses", "esta", "estas",
    "este", "estes", "eu", "foi", "fomos", "for", "foram", "há", "isso", "isto", "já", "lhe",
    "lhes", "mais", "mas", "me", "mesmo", "meu", "meus", "minha", "minhas", "muito", "na", "nas",
    "no", "nos", "nós", "nossa", "nossas", "nosso", "nossos", "num", "numa", "não", "o", "os",
    "ou", "para", "pela", "pelas", "pelo", "pelos", "por", "qual", "quando", "que", "quem", "se",
    "sem", "seu", "seus", "sua", "suas", "são", "só", "também", "te", "tem", "tendo", "teu",
    "teus", "tu", "tua", "tuas", "um", "uma", "umas", "uns", "você", "vocês", "vos",
];

// ---------------------------------------------------------------------------
// Analyzer registration
// ---------------------------------------------------------------------------

/// Register all custom analyzers on a Tantivy [`Index`].
///
/// This must be called after creating the index and before writing or searching.
/// The analyzers are registered on the index's tokenizer manager and become
/// available to any field that references them by name.
///
/// # Registered analyzers
///
/// | Name          | Behaviour                                              |
/// |---------------|--------------------------------------------------------|
/// | `"standard"`  | SimpleTokenizer → LowerCaser → English stop words      |
/// | `"keyword"`   | RawTokenizer (no tokenization, exact match)            |
/// | `"ngram"`     | NgramTokenizer(2, 3) → LowerCaser                     |
/// | `"portuguese"`| SimpleTokenizer → LowerCaser → Portuguese stop words   |
/// | `"cjk"`       | jieba-rs Chinese word segmenter → LowerCaser           |
pub fn register_analyzers(index: &Index) {
    let manager = index.tokenizers();

    // -- standard: lowercase + English stop words --
    let stop_words: Vec<String> = ENGLISH_STOP_WORDS.iter().map(|s| s.to_string()).collect();
    let standard = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(100))
        .filter(LowerCaser)
        .filter(StopWordFilter::remove(stop_words))
        .build();
    manager.register("standard", standard);

    // -- keyword: no tokenization (raw) --
    let keyword = TextAnalyzer::builder(RawTokenizer::default())
        .filter(RemoveLongFilter::limit(256))
        .build();
    manager.register("keyword", keyword);

    // -- ngram: partial matching with n-grams (min=2, max=3) --
    let ngram = TextAnalyzer::builder(NgramTokenizer::all_ngrams(2, 3).expect("valid ngram range"))
        .filter(LowerCaser)
        .build();
    manager.register("ngram", ngram);

    // -- portuguese: lowercase + Portuguese stop words --
    let pt_stop_words: Vec<String> = PORTUGUESE_STOP_WORDS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let portuguese = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(100))
        .filter(LowerCaser)
        .filter(StopWordFilter::remove(pt_stop_words))
        .build();
    manager.register("portuguese", portuguese);

    // -- cjk: Chinese/Japanese/Korean tokenizer via jieba-rs --
    let cjk = TextAnalyzer::builder(JiebaTokenizer)
        .filter(LowerCaser)
        .build();
    manager.register("cjk", cjk);
}

// ---------------------------------------------------------------------------
// JiebaTokenizer — bridge jieba-rs to Tantivy
// ---------------------------------------------------------------------------

/// A Tantivy [`Tokenizer`] backed by jieba-rs for CJK text segmentation.
///
/// jieba-rs is a port of the popular Python `jieba` library, using a
/// dictionary-based approach with HMM for unknown words.
#[derive(Clone)]
struct JiebaTokenizer;

impl tantivy::tokenizer::Tokenizer for JiebaTokenizer {
    type TokenStream<'a> = JiebaTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let jieba = jieba_rs::Jieba::new();

        // Build a mapping from Unicode char index -> byte offset for the input text.
        // jieba-rs returns Unicode character positions, but Tantivy expects byte offsets.
        let char_byte_offsets: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        let text_byte_len = text.len();

        let words: Vec<(String, usize, usize)> = jieba
            .tokenize(text, jieba_rs::TokenizeMode::Search, true)
            .into_iter()
            .map(|t| {
                let byte_start = char_byte_offsets
                    .get(t.start)
                    .copied()
                    .unwrap_or(text_byte_len);
                let byte_end = char_byte_offsets
                    .get(t.end)
                    .copied()
                    .unwrap_or(text_byte_len);
                let word = t.word.to_owned();
                (word, byte_start, byte_end)
            })
            .collect();

        JiebaTokenStream {
            tokens: words,
            index: 0,
            token: tantivy::tokenizer::Token::default(),
        }
    }
}

/// Token stream produced by [`JiebaTokenizer`].
struct JiebaTokenStream {
    tokens: Vec<(String, usize, usize)>,
    index: usize,
    token: tantivy::tokenizer::Token,
}

impl tantivy::tokenizer::TokenStream for JiebaTokenStream {
    fn advance(&mut self) -> bool {
        if self.index < self.tokens.len() {
            let (ref word, start, end) = self.tokens[self.index];
            self.token.text.clear();
            self.token.text.push_str(word);
            self.token.offset_from = start;
            self.token.offset_to = end;
            self.token.position = self.index;
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &tantivy::tokenizer::Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut tantivy::tokenizer::Token {
        &mut self.token
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::schema::SchemaBuilder;

    fn make_test_index() -> Index {
        let schema = SchemaBuilder::default().build();
        let index = Index::create_in_ram(schema);
        register_analyzers(&index);
        index
    }

    /// Helper: tokenize text with a named analyzer and return token strings.
    fn tokenize(index: &Index, analyzer_name: &str, text: &str) -> Vec<String> {
        let tokenizer = index.tokenizers();
        let mut analyzer = tokenizer
            .get(analyzer_name)
            .unwrap_or_else(|| panic!("analyzer '{}' not found", analyzer_name));

        let mut tokens = Vec::new();
        let mut stream = analyzer.token_stream(text);
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }

    #[test]
    fn standard_analyzer_lowercases() {
        let index = make_test_index();
        let tokens = tokenize(&index, "standard", "Hello World");
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn standard_analyzer_removes_stop_words() {
        let index = make_test_index();
        let tokens = tokenize(&index, "standard", "the quick and the dead");
        // "the" and "and" are stop words
        assert!(!tokens.contains(&"the".to_string()));
        assert!(!tokens.contains(&"and".to_string()));
        assert!(tokens.contains(&"quick".to_string()));
        assert!(tokens.contains(&"dead".to_string()));
    }

    #[test]
    fn keyword_analyzer_no_tokenization() {
        let index = make_test_index();
        let tokens = tokenize(&index, "keyword", "Hello World");
        assert_eq!(tokens, vec!["Hello World"]);
    }

    #[test]
    fn ngram_analyzer_produces_substrings() {
        let index = make_test_index();
        let tokens = tokenize(&index, "ngram", "abc");
        // For "abc" with min=2, max=3: "ab", "abc", "bc"
        assert!(tokens.contains(&"ab".to_string()));
        assert!(tokens.contains(&"bc".to_string()));
        assert!(tokens.contains(&"abc".to_string()));
    }

    #[test]
    fn portuguese_analyzer_removes_stop_words() {
        let index = make_test_index();
        let tokens = tokenize(&index, "portuguese", "o gato e o rato");
        // "o" and "e" are Portuguese stop words
        assert!(!tokens.contains(&"o".to_string()));
        assert!(!tokens.contains(&"e".to_string()));
        assert!(tokens.contains(&"gato".to_string()));
        assert!(tokens.contains(&"rato".to_string()));
    }

    #[test]
    fn cjk_analyzer_segments_chinese() {
        let index = make_test_index();
        let tokens = tokenize(&index, "cjk", "中华人民共和国");
        // jieba should segment this into meaningful Chinese words
        assert!(!tokens.is_empty());
        // It should produce multiple tokens for this compound phrase
        assert!(tokens.len() > 1);
    }
}
