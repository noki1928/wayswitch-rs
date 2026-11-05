//! Языковые модели: частотный словарь и символьные триграммы.
//!
//! Данные — те же файлы, что у WaySwitch (FrequencyWords, CC-BY-SA 4.0),
//! вшиты в бинарник: демону не нужно ничего читать с диска при старте.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};

use flate2::read::GzDecoder;
use serde::Deserialize;

const RU_WORDS: &[u8] = include_bytes!("../data/ru.words.gz");
const EN_WORDS: &[u8] = include_bytes!("../data/en.words.gz");
const RU_NGRAMS: &[u8] = include_bytes!("../data/ru.ngrams.json.gz");
const EN_NGRAMS: &[u8] = include_bytes!("../data/en.ngrams.json.gz");

const PAD_START: char = '^';
const PAD_END: char = '$';
const TOP_N: usize = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    Ru,
    En,
}

impl Lang {
    pub fn letters(self) -> &'static str {
        match self {
            Lang::Ru => crate::keymap::CYRILLIC,
            Lang::En => crate::keymap::LATIN,
        }
    }

    pub fn has_letter(self, c: char) -> bool {
        c.to_lowercase().all(|l| self.letters().contains(l))
    }
}

#[derive(Deserialize)]
struct NgramFile {
    tri: HashMap<String, f64>,
    ctx: HashMap<String, f64>,
    floor: f64,
}

pub struct LanguageModel {
    words: HashSet<String>,
    sorted: Vec<String>,
    top: HashSet<String>,
    endings: HashSet<String>,
    ngrams: NgramFile,
}

impl LanguageModel {
    pub fn builtin(lang: Lang) -> Self {
        let (words, ngrams) = match lang {
            Lang::Ru => (RU_WORDS, RU_NGRAMS),
            Lang::En => (EN_WORDS, EN_NGRAMS),
        };
        let mut list = Vec::new();
        for line in BufReader::new(GzDecoder::new(words)).lines().map_while(Result::ok) {
            let word = line.split('\t').next().unwrap_or("").to_string();
            if !word.is_empty() {
                list.push(word);
            }
        }
        let mut json = String::new();
        GzDecoder::new(ngrams).read_to_string(&mut json).expect("встроенные n-граммы");
        let ngrams: NgramFile = serde_json::from_str(&json).expect("встроенные n-граммы");
        Self::from_parts(list, ngrams)
    }

    fn from_parts(list: Vec<String>, ngrams: NgramFile) -> Self {
        let top = list.iter().take(TOP_N).cloned().collect();
        // Окончание считается обычным, если им кончаются хотя бы три слова.
        let mut counts: HashMap<String, u32> = HashMap::new();
        for w in &list {
            let chars: Vec<char> = w.chars().collect();
            if chars.len() >= 3 {
                *counts.entry(chars[chars.len() - 2..].iter().collect()).or_default() += 1;
            }
        }
        let endings = counts.into_iter().filter(|(_, n)| *n >= 3).map(|(e, _)| e).collect();
        let mut sorted = list.clone();
        sorted.sort();
        sorted.dedup();
        LanguageModel { words: list.into_iter().collect(), sorted, top, endings, ngrams }
    }

    pub fn is_word(&self, word: &str) -> bool {
        self.words.contains(&word.to_lowercase())
    }

    pub fn is_top(&self, word: &str) -> bool {
        self.top.contains(&word.to_lowercase())
    }

    pub fn has_ending(&self, word: &str) -> bool {
        let chars: Vec<char> = word.chars().collect();
        chars.len() >= 2 && self.endings.contains(&chars[chars.len() - 2..].iter().collect::<String>())
    }

    pub fn prefix_count(&self, prefix: &str, limit: usize) -> usize {
        let prefix = prefix.to_lowercase();
        if prefix.is_empty() {
            return 0;
        }
        let start = self.sorted.partition_point(|w| w.as_str() < prefix.as_str());
        self.sorted[start..].iter().take(limit).take_while(|w| w.starts_with(&prefix)).count()
    }

    pub fn has_prefix(&self, prefix: &str) -> bool {
        self.prefix_count(prefix, 1) > 0
    }

    /// Слово или его форма: отбрасываем до двух последних букв («сонату» → «соната»).
    pub fn looks_like_word(&self, word: &str) -> bool {
        if self.is_word(word) {
            return true;
        }
        let chars: Vec<char> = word.chars().collect();
        (1..=2).any(|cut| chars.len() >= 3 + cut && self.has_prefix(&chars[..chars.len() - cut].iter().collect::<String>()))
    }

    /// Средняя логвероятность символа при двух предыдущих.
    pub fn score(&self, text: &str) -> f64 {
        let mut s: Vec<char> = vec![PAD_START, PAD_START];
        s.extend(text.to_lowercase().chars());
        s.push(PAD_END);
        let mut total = 0.0;
        let mut n = 0;
        let mut key = String::with_capacity(12);
        for i in 2..s.len() {
            key.clear();
            key.extend(&s[i - 2..=i]);
            let lp = match self.ngrams.tri.get(&key) {
                Some(&lp) => lp,
                None => {
                    key.pop();
                    *self.ngrams.ctx.get(&key).unwrap_or(&self.ngrams.floor)
                }
            };
            total += lp;
            n += 1;
        }
        if n == 0 {
            self.ngrams.floor
        } else {
            total / n as f64
        }
    }
}

pub struct Models {
    pub ru: LanguageModel,
    pub en: LanguageModel,
}

impl Models {
    pub fn builtin() -> Self {
        let ru = std::thread::spawn(|| LanguageModel::builtin(Lang::Ru));
        let en = LanguageModel::builtin(Lang::En);
        Models { ru: ru.join().expect("словарь ru"), en }
    }

    pub fn get(&self, lang: Lang) -> &LanguageModel {
        match lang {
            Lang::Ru => &self.ru,
            Lang::En => &self.en,
        }
    }
}
