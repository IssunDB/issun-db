use crate::schema::Language;
use rust_stemmers::{Algorithm, Stemmer};
use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;

pub const STOP_WORDS_EN: &[&str] = &[
    "a",
    "about",
    "above",
    "after",
    "again",
    "against",
    "all",
    "am",
    "an",
    "and",
    "any",
    "are",
    "aren't",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can't",
    "cannot",
    "could",
    "couldn't",
    "did",
    "didn't",
    "do",
    "does",
    "doesn't",
    "doing",
    "don't",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "had",
    "hadn't",
    "has",
    "hasn't",
    "have",
    "haven't",
    "having",
    "he",
    "he'd",
    "he'll",
    "he's",
    "her",
    "here",
    "here's",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "how's",
    "i",
    "i'd",
    "i'll",
    "i'm",
    "i've",
    "if",
    "in",
    "into",
    "is",
    "isn't",
    "it",
    "it's",
    "its",
    "itself",
    "let's",
    "me",
    "more",
    "most",
    "mustn't",
    "my",
    "myself",
    "no",
    "nor",
    "not",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "ought",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "same",
    "shan't",
    "she",
    "she'd",
    "she'll",
    "she's",
    "should",
    "shouldn't",
    "so",
    "some",
    "such",
    "than",
    "that",
    "that's",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "there's",
    "these",
    "they",
    "they'd",
    "they'll",
    "they're",
    "they've",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "very",
    "was",
    "wasn't",
    "we",
    "we'd",
    "we'll",
    "we're",
    "we've",
    "were",
    "weren't",
    "what",
    "what's",
    "when",
    "when's",
    "where",
    "where's",
    "which",
    "while",
    "who",
    "who's",
    "whom",
    "why",
    "why's",
    "with",
    "won't",
    "would",
    "wouldn't",
    "you",
    "you'd",
    "you'll",
    "you're",
    "you've",
    "your",
    "yours",
    "yourself",
    "yourselves",
];

pub const STOP_WORDS_ES: &[&str] = &[
    "el", "la", "los", "las", "un", "una", "unos", "unas", "y", "o", "pero", "si", "de", "del",
    "a", "al", "en", "con", "por", "para", "como", "es", "esta", "fue", "son", "sus", "mi", "tu",
    "su", "nos", "me", "te", "se", "lo", "le", "que", "este", "esta", "aquel", "ella", "ellos",
    "ellas", "nosotros", "vosotros", "mi", "mis", "tus", "sus", "muy", "tambien", "como", "donde",
];

pub const STOP_WORDS_FR: &[&str] = &[
    "le", "la", "les", "un", "une", "des", "et", "ou", "mais", "si", "de", "du", "des", "à", "au",
    "aux", "en", "avec", "par", "pour", "comme", "est", "sont", "était", "été", "mon", "ton",
    "son", "mes", "tes", "ses", "nous", "vous", "il", "elle", "ils", "elles", "se", "y", "en",
    "que", "qui", "ce", "cette", "dans", "sur", "pas", "plus", "très", "mais", "donc", "ou",
];

pub const STOP_WORDS_DE: &[&str] = &[
    "der", "die", "das", "ein", "eine", "einer", "eines", "einem", "einen", "und", "oder", "aber",
    "wenn", "von", "vom", "zu", "zum", "zur", "in", "im", "mit", "für", "wie", "ist", "sind",
    "war", "gewesen", "mein", "dein", "sein", "ihr", "wir", "ihr", "sie", "es", "er", "sich",
    "dass", "was", "wer", "wie", "aus", "bei", "nach", "um", "am", "an", "als", "auch", "so",
];

pub const STOP_WORDS_IT: &[&str] = &[
    "il", "la", "i", "gli", "le", "un", "uno", "una", "e", "o", "ma", "se", "di", "da", "in",
    "con", "su", "per", "tra", "fra", "come", "è", "sono", "era", "stato", "il mio", "mio", "tuo",
    "suo", "nostro", "vostro", "loro", "mi", "ti", "si", "ci", "vi", "lo", "gli", "che", "chi",
    "questo", "quello", "anche", "più", "non", "perché", "ed", "ad",
];

pub const STOP_WORDS_PT: &[&str] = &[
    "o", "a", "os", "as", "um", "uma", "uns", "umas", "e", "ou", "mas", "se", "de", "do", "da",
    "dos", "das", "em", "no", "na", "nos", "nas", "com", "por", "para", "como", "é", "são", "era",
    "foi", "meu", "teu", "seu", "nosso", "vosso", "me", "te", "se", "lhe", "o", "a", "que", "quem",
    "este", "esta", "aquele", "também", "mais", "não", "porque", "com", "sem",
];

fn get_stop_words(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::English => STOP_WORDS_EN,
        Language::Spanish => STOP_WORDS_ES,
        Language::French => STOP_WORDS_FR,
        Language::German => STOP_WORDS_DE,
        Language::Italian => STOP_WORDS_IT,
        Language::Portuguese => STOP_WORDS_PT,
    }
}

fn map_algorithm(lang: Language) -> Algorithm {
    match lang {
        Language::English => Algorithm::English,
        Language::Spanish => Algorithm::Spanish,
        Language::French => Algorithm::French,
        Language::German => Algorithm::German,
        Language::Italian => Algorithm::Italian,
        Language::Portuguese => Algorithm::Portuguese,
    }
}

/// Map a single Unicode character with diacritics to its ASCII base.
///
/// Returns `Some(&'static str)` when a mapping is known, `None` otherwise.
fn fold_char(c: char) -> Option<&'static str> {
    match c {
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => {
            Some("a")
        }
        'Æ' | 'æ' => Some("ae"),
        'Ç' | 'ç' => Some("c"),
        'È' | 'É' | 'Ê' | 'Ë' | 'è' | 'é' | 'ê' | 'ë' => Some("e"),
        'Ì' | 'Í' | 'Î' | 'Ï' | 'ì' | 'í' | 'î' | 'ï' => Some("i"),
        'Ð' | 'ð' => Some("d"),
        'Ñ' | 'ñ' => Some("n"),
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => {
            Some("o")
        }
        'Ù' | 'Ú' | 'Û' | 'Ü' | 'ù' | 'ú' | 'û' | 'ü' => Some("u"),
        'Ý' | 'ý' | 'ÿ' => Some("y"),
        'ß' => Some("ss"),
        'Þ' | 'þ' => Some("th"),
        _ => None,
    }
}

/// Fold diacritics to their ASCII base characters.
///
/// For example, "café" becomes "cafe" and "über" becomes "uber". This
/// normalization runs before tokenization so that accented and unaccented
/// spellings of the same word produce identical index terms.
pub fn fold_ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match fold_char(c) {
            Some(s) => out.push_str(s),
            None => out.push(c),
        }
    }
    out
}

/// Dynamic multi-language tokenizer that splits string properties using Unicode word boundaries,
/// downcases terms, filters out language-specific stop words, and applies Snowball stemming.
///
/// Diacritics are folded to their ASCII base before segmentation so that
/// "café" and "cafe" produce the same stem.
pub fn tokenize(text: &str, lang: Language) -> HashMap<String, u32> {
    let folded = fold_ascii(text);
    let mut terms = HashMap::new();
    let words = folded.unicode_words();
    let stop_words = get_stop_words(lang);
    let stemmer = Stemmer::create(map_algorithm(lang));

    for word in words {
        let lower = word.to_lowercase();
        if !stop_words.contains(&lower.as_str()) {
            let stemmed = stemmer.stem(&lower);
            *terms.entry(stemmed.into_owned()).or_insert(0) += 1;
        }
    }
    terms
}
