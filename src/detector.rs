//! Решение «слово набрано не в той раскладке?» по двум гипотезам.
//!
//! A — то, что сейчас на экране (декодировка в текущей группе), B — те же
//! нажатия в другой группе. Правила перенесены из WaySwitch без изменений
//! логики; отличие одно — текст нигде не попадает в `reason`, только числа.

use std::collections::HashSet;

use crate::keymap::{Alphabet, KeyPress, Keymap, CYRILLIC, LATIN};
use crate::lang::{Lang, Models};
use crate::secure::{LearnedStore, SecretText};

const INNER_MARKS: [char; 2] = ['-', '\''];
const URL_SCHEMES: [&str; 3] = ["http://", "https://", "ftp://"];
const TECH_PREFIXES: [&str; 6] = ["http://", "https://", "ftp://", "ssh://", "www.", "git@"];
const TECH_MARKERS: [&str; 9] = ["://", "@", "--", ".com", ".ru", ".org", ".net", ".io", ".dev"];
const PATH_MARKERS: [char; 3] = ['/', '\\', '~'];
const TECH_WORDS: [&str; 9] = ["http", "https", "ftp", "www", "ssh", "git", "sudo", "apt", "dnf"];
const NOT_BEFORE_DOT: &str = "аеёиоуыэюяьъй";
const ABBREVIATIONS: [&str; 28] = [
    "руб", "коп", "ул", "г", "гг", "др", "стр", "см", "тыс", "млн", "млрд", "т", "пр", "проч",
    "д", "кв", "обл", "р", "ед", "шт", "мин", "сек", "мес", "англ", "рус", "напр", "доп", "тел",
];
const PRONOUNS: [&str; 30] = [
    "что", "кто", "где", "куда", "откуда", "как", "когда", "какой", "какая", "какое", "какие",
    "каких", "каким", "какую", "чей", "чья", "чьё", "чьи", "почему", "зачем", "отчего",
    "сколько", "чего", "чему", "чем", "кого", "кому", "кем", "ком", "чём",
];
const PERSONAL: [&str; 8] = ["я", "ты", "он", "она", "оно", "мы", "вы", "они"];
const KA_STEMS: [&str; 26] = [
    "давай", "давайте", "дай", "дайте", "смотри", "смотрите", "ну", "иди", "идите", "скажи",
    "скажите", "глянь", "погоди", "подожди", "посмотри", "покажи", "попробуй", "принеси",
    "сходи", "налей", "расскажи", "подумай", "постой", "пойди", "напиши", "позвони",
];
const TAKI_STEMS: [&str; 7] = ["всё", "все", "так", "опять", "наконец", "довольно", "есть"];
const ZHE_EXTRA: [&str; 30] = [
    "тот", "та", "те", "так", "там", "тут", "туда", "сюда", "здесь", "сейчас", "тогда", "всё",
    "все", "это", "этот", "эта", "эти", "такой", "такая", "такое", "такие", "сразу", "опять",
    "ведь", "нет", "да", "его", "её", "их", "ему",
];
const ZHE_EXTRA2: [&str; 6] = ["ей", "им", "меня", "тебя", "нас", "вас"];
const BY_EXTRA: [&str; 14] = [
    "был", "была", "было", "были", "мог", "могла", "могли", "смог", "если", "хотя", "тогда",
    "лучше", "хорошо", "можно",
];
const BY_EXTRA2: [&str; 2] = ["нужно", "надо"];
const BY_VERB_ENDINGS: [&str; 9] = ["ал", "ял", "ел", "ил", "ыл", "ул", "ла", "ло", "ли"];
const LI_VERB_ENDINGS: [&str; 13] =
    ["ешь", "ишь", "ете", "ите", "ет", "ит", "ут", "ют", "ат", "ят", "ть", "ся", "сь"];
const LI_WORDS: [&str; 15] = [
    "то", "есть", "был", "была", "было", "были", "так", "верно", "можно", "нужно", "надо",
    "много", "мало", "скоро", "давно",
];
const BRAND_PREFIXES: [&str; 30] = [
    "мега", "сбер", "авто", "мото", "фото", "видео", "теле", "радио", "супер", "мини", "макси",
    "евро", "гига", "кило", "нано", "вело", "аэро", "агро", "био", "эко", "инфо", "медиа",
    "пром", "газ", "тех", "спец", "рос", "мос", "яндекс", "вкус",
];

pub const EARLY_LEN: usize = 4;
const EARLY_MIN_WORDS: usize = 3;

#[derive(Clone, Copy, Debug)]
pub struct Sensitivity {
    pub min_length: usize,
    pub margin: f64,
    pub floor: f64,
}

impl Sensitivity {
    pub fn named(name: &str) -> Self {
        match name {
            "conservative" => Sensitivity { min_length: 4, margin: 2.5, floor: -5.5 },
            "aggressive" => Sensitivity { min_length: 3, margin: 0.8, floor: -7.0 },
            _ => Sensitivity { min_length: 3, margin: 1.5, floor: -6.0 },
        }
    }
}

/// Слова, которые не трогаем: свои (открытым текстом, их пишет человек) и
/// выученные по отменам (только HMAC).
#[derive(Default)]
pub struct Exceptions {
    pub plain: HashSet<String>,
    pub learned: Option<LearnedStore>,
}

impl Exceptions {
    pub fn contains(&self, word: &str) -> bool {
        let low = word.to_lowercase();
        self.plain.contains(&low) || self.learned.as_ref().is_some_and(|l| l.contains(&low))
    }

    fn any_starts_with(&self, prefix: &str) -> bool {
        self.plain.iter().any(|e| e.starts_with(prefix))
    }
}

pub struct Hypothesis {
    pub text: SecretText,
    pub core: SecretText,
    pub tail: SecretText,
    pub lang: Option<Lang>,
    pub valid: bool,
    pub in_dict: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Keep,
    Fix,
}

pub struct Decision {
    pub action: Action,
    pub target: SecretText,
    /// Причина для журнала. Только константы и числа — никакого текста.
    pub reason: String,
    pub a_lang: Option<Lang>,
    pub a_in_dict: bool,
    pub b_lang: Option<Lang>,
}

pub struct Detector {
    pub keymap: Keymap,
    pub models: Models,
    pub exceptions: Exceptions,
    pub sensitivity: Sensitivity,
}

fn lower(s: &str) -> String {
    s.to_lowercase()
}

fn py_islower(s: &str) -> bool {
    let mut cased = false;
    for c in s.chars() {
        if c.is_uppercase() {
            return false;
        }
        cased |= c.is_lowercase();
    }
    cased
}

fn py_isupper(s: &str) -> bool {
    let mut cased = false;
    for c in s.chars() {
        if c.is_lowercase() {
            return false;
        }
        cased |= c.is_uppercase();
    }
    cased
}

fn split_tail(text: &str, lang: Lang) -> (String, String) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = chars.len();
    while i > 0 && !lang.has_letter(chars[i - 1]) && !INNER_MARKS.contains(&chars[i - 1]) {
        i -= 1;
    }
    (chars[..i].iter().collect(), chars[i..].iter().collect())
}

fn inner(core: &str, lang: Lang) -> String {
    core.chars().skip_while(|&c| !lang.has_letter(c)).collect()
}

fn is_camel(core: &str) -> bool {
    core.chars().skip(1).any(char::is_uppercase) && core.chars().any(char::is_lowercase)
}

fn mixed_alphabets(core: &str) -> bool {
    let low = lower(core);
    low.chars().any(|c| CYRILLIC.contains(c)) && low.chars().any(|c| LATIN.contains(c))
}

fn all_letters(text: &str, lang: Lang) -> bool {
    text.chars().all(|c| lang.has_letter(c))
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

impl Detector {
    pub fn decode(&self, keys: &[KeyPress], group: usize) -> SecretText {
        self.keymap.decode(keys, group)
    }

    pub fn lang_of_group(&self, group: usize) -> Option<Lang> {
        match self.keymap.alphabet(group) {
            Alphabet::Russian => Some(Lang::Ru),
            Alphabet::Latin => Some(Lang::En),
            Alphabet::Other => None,
        }
    }

    fn hypothesis(&self, keys: &[KeyPress], group: usize) -> Hypothesis {
        let text = self.decode(keys, group);
        let Some(lang) = self.lang_of_group(group) else {
            return Hypothesis {
                core: text.clone(),
                text,
                tail: SecretText::default(),
                lang: None,
                valid: false,
                in_dict: false,
            };
        };
        let (core, tail) = split_tail(text.expose(), lang);
        let low = lower(&core);
        let valid = !low.is_empty()
            && low.chars().all(|c| lang.has_letter(c) || INNER_MARKS.contains(&c))
            && !INNER_MARKS.contains(&low.chars().next().unwrap())
            && !INNER_MARKS.contains(&low.chars().last().unwrap());
        let in_dict = valid && self.models.get(lang).is_word(&low);
        Hypothesis { text, core: core.into(), tail: tail.into(), lang: Some(lang), valid, in_dict }
    }

    /// Слово в другой раскладке — ровно схема URL: исправить сразу.
    pub fn early_url_target(&self, keys: &[KeyPress], other: usize) -> Option<SecretText> {
        if self.lang_of_group(other) != Some(Lang::En) {
            return None;
        }
        let b = self.decode(keys, other);
        URL_SCHEMES.contains(&lower(b.expose()).as_str()).then_some(b)
    }

    /// «пирогб» → «пирог,»: знак набран буквой из другой раскладки.
    pub fn punct_fix(&self, keys: &[KeyPress], current: usize, other: usize) -> Option<char> {
        if keys.len() < 3 || self.lang_of_group(current) != Some(Lang::Ru) {
            return None;
        }
        let last = keys.last()?;
        if last.shift {
            return None;
        }
        let mark = self.keymap.char(last.code, other, false, false)?;
        if mark != ',' && mark != '.' {
            return None;
        }
        let model = self.models.get(Lang::Ru);
        let stem = lower(self.decode(&keys[..keys.len() - 1], current).expose());
        let full = lower(self.decode(keys, current).expose());
        if stem.is_empty() || !all_letters(&stem, Lang::Ru) || model.is_word(&full) {
            return None;
        }
        if mark == '.' && ABBREVIATIONS.contains(&stem.as_str()) {
            return Some(mark);
        }
        if mark == '.' && NOT_BEFORE_DOT.contains(stem.chars().last()?) {
            return None;
        }
        if !model.is_word(&stem) || char_len(&stem) < 3 || model.has_ending(&full) {
            return None;
        }
        Some(mark)
    }

    /// «чтото» → «что-то», «кембы» → «кем бы».
    pub fn particles_fix(&self, keys: &[KeyPress], current: usize) -> Option<SecretText> {
        if self.lang_of_group(current) != Some(Lang::Ru) {
            return None;
        }
        let model = self.models.get(Lang::Ru);
        let text = lower(self.decode(keys, current).expose());
        if char_len(&text) < 4 || !all_letters(&text, Lang::Ru) || model.is_word(&text) {
            return None;
        }
        let stem_of = |p: &str| text.strip_suffix(p).map(str::to_string);
        let in_list = |s: &str, lists: &[&[&str]]| lists.iter().any(|l| l.contains(&s));
        let rules: [(&str, &[&[&str]]); 6] = [
            ("то", &[&PRONOUNS]),
            ("либо", &[&PRONOUNS]),
            ("нибудь", &[&PRONOUNS]),
            ("ка", &[&KA_STEMS]),
            ("таки", &[&TAKI_STEMS]),
            ("же", &[&PRONOUNS, &PERSONAL, &ZHE_EXTRA, &ZHE_EXTRA2]),
        ];
        for (particle, lists) in rules {
            if let Some(stem) = stem_of(particle) {
                if in_list(&stem, lists) {
                    let sep = if particle == "же" { ' ' } else { '-' };
                    return Some(format!("{stem}{sep}{particle}").into());
                }
            }
        }
        if let Some(stem) = stem_of("бы") {
            if in_list(&stem, &[&PRONOUNS, &PERSONAL, &BY_EXTRA, &BY_EXTRA2])
                || (char_len(&stem) >= 5
                    && BY_VERB_ENDINGS.iter().any(|e| stem.ends_with(e))
                    && model.is_word(&stem))
            {
                return Some(format!("{stem} бы").into());
            }
        }
        if let Some(stem) = stem_of("ли") {
            if LI_WORDS.contains(&stem.as_str())
                || (char_len(&stem) >= 4
                    && LI_VERB_ENDINGS.iter().any(|e| stem.ends_with(e))
                    && model.is_word(&stem))
            {
                return Some(format!("{stem} ли").into());
            }
        }
        None
    }

    /// Начало слова явно в другой раскладке («ghbd» → «прив»): переключить сразу.
    pub fn early_word_target(&self, keys: &[KeyPress], current: usize, other: usize) -> Option<SecretText> {
        if keys.len() != EARLY_LEN {
            return None;
        }
        let (cur, oth) = (self.lang_of_group(current)?, self.lang_of_group(other)?);
        if cur == oth {
            return None;
        }
        let a = self.decode(keys, current);
        let b = self.decode(keys, other);
        for (text, lang) in [(a.expose(), cur), (b.expose(), oth)] {
            let rest: String = text.chars().skip(1).collect();
            if !all_letters(text, lang) || !py_islower(&rest) {
                return None;
            }
        }
        let start = lower(a.expose());
        if self.exceptions.any_starts_with(&start) || self.models.get(cur).has_prefix(&start) {
            return None;
        }
        if self.models.get(oth).prefix_count(&lower(b.expose()), EARLY_MIN_WORDS) < EARLY_MIN_WORDS {
            return None;
        }
        Some(b)
    }

    /// «пРИВЕТ» → «Привет» (Caps Lock включён нечаянно). Возвращает текст и группу.
    pub fn caps_lock_fix(&self, keys: &[KeyPress], current: usize, other: usize) -> Option<(SecretText, usize)> {
        if keys.len() < 3 || !keys.iter().all(|k| k.caps) {
            return None;
        }
        for group in [current, other] {
            let Some(lang) = self.lang_of_group(group) else { continue };
            let text = self.decode(keys, group);
            let t = text.expose();
            if !all_letters(t, lang) {
                continue;
            }
            let first = t.chars().next()?;
            let rest: String = t.chars().skip(1).collect();
            if !(first.is_lowercase() && py_isupper(&rest)) {
                continue;
            }
            let fixed: String = first.to_uppercase().chain(lower(&rest).chars()).collect();
            if self.models.get(lang).looks_like_word(&lower(&fixed)) {
                return Some((fixed.into(), group));
            }
        }
        None
    }

    /// «ПРивет» → «Привет»: Shift отпущен на букву позже.
    pub fn two_capitals_fix(&self, keys: &[KeyPress], current: usize) -> Option<SecretText> {
        let lang = self.lang_of_group(current)?;
        let text = self.decode(keys, current);
        let chars: Vec<char> = text.expose().chars().collect();
        if chars.len() < 4 || !all_letters(text.expose(), lang) {
            return None;
        }
        let tail: String = chars[2..].iter().collect();
        if !(chars[0].is_uppercase() && chars[1].is_uppercase() && py_islower(&tail)) {
            return None;
        }
        let fixed: String = std::iter::once(chars[0]).chain(lower(&chars[1..].iter().collect::<String>()).chars()).collect();
        self.models.get(lang).is_word(&lower(&fixed)).then(|| fixed.into())
    }

    /// «сонатуБаха» → «сонату Баха».
    pub fn split_words_fix(&self, keys: &[KeyPress], current: usize) -> Option<SecretText> {
        if self.lang_of_group(current) != Some(Lang::Ru) {
            return None;
        }
        let model = self.models.get(Lang::Ru);
        let text = self.decode(keys, current);
        let chars: Vec<char> = text.expose().chars().collect();
        if chars.len() < 6 || !chars[0].is_lowercase() || !all_letters(text.expose(), Lang::Ru) {
            return None;
        }
        if model.is_word(&lower(text.expose())) {
            return None;
        }
        let cuts: Vec<usize> = (1..chars.len() - 1)
            .filter(|&i| chars[i - 1].is_lowercase() && chars[i].is_uppercase())
            .collect();
        let [cut] = cuts[..] else { return None };
        let left: String = chars[..cut].iter().collect();
        let right: String = chars[cut..].iter().collect();
        if char_len(&left) < 3 || char_len(&right) < 3 || BRAND_PREFIXES.contains(&lower(&left).as_str()) {
            return None;
        }
        let (l, r) = (lower(&left), lower(&right));
        if !model.looks_like_word(&l) || !model.looks_like_word(&r) {
            return None;
        }
        if !(model.is_word(&l) || model.is_word(&r)) {
            return None;
        }
        Some(format!("{left} {right}").into())
    }

    pub fn decide(&self, keys: &[KeyPress], current: usize, other: usize, context: (Option<Lang>, Option<Lang>)) -> Decision {
        let a = self.hypothesis(keys, current);
        let b = self.hypothesis(keys, other);
        let sens = self.sensitivity;
        let make = |action, target: SecretText, reason: String| Decision {
            action,
            target,
            reason,
            a_lang: a.lang,
            a_in_dict: a.in_dict,
            b_lang: b.lang,
        };
        let keep = |reason: &str| make(Action::Keep, a.text.clone(), reason.to_string());
        let fix = |reason: String| {
            make(Action::Fix, format!("{}{}", b.core.expose(), b.tail.expose()).into(), reason)
        };

        let (Some(a_lang), Some(b_lang)) = (a.lang, b.lang) else {
            return keep("unknown-alphabet");
        };

        // Технический токен в B: URL, набранный не в той раскладке.
        let bl = lower(b.text.expose());
        if (TECH_PREFIXES.iter().any(|p| bl.starts_with(p)) || bl.contains("://")) && !a.in_dict && b_lang == Lang::En {
            return make(Action::Fix, b.text.clone(), "url".into());
        }

        // Стоп-правила по тому, что на экране.
        let at = a.text.expose();
        let core = a.core.expose();
        if at.chars().any(|c| c.is_ascii_digit()) {
            return keep("digits");
        }
        if mixed_alphabets(core) {
            return keep("mixed");
        }
        if is_camel(core) {
            return keep("camel");
        }
        let al = lower(at);
        let inner_core = inner(core, a_lang);
        if TECH_MARKERS.iter().any(|m| al.contains(m))
            || inner_core.contains(&PATH_MARKERS[..])
            || TECH_PREFIXES.iter().any(|p| al.starts_with(p))
            || TECH_WORDS.contains(&lower(core).as_str())
        {
            return keep("tech");
        }
        let core_len = char_len(core);
        if (2..5).contains(&core_len) && py_isupper(core) {
            return keep("abbrev");
        }
        if self.exceptions.contains(core) {
            return keep("exception");
        }

        if !a.valid && !b.valid {
            return keep("invalid");
        }
        if a.valid && a.in_dict {
            return keep("dict-a");
        }
        let b_len = char_len(b.core.expose());
        let short = b_len < sens.min_length;
        if b.valid && b.in_dict && !short {
            return fix("dict-b".into());
        }
        if short {
            if b.valid && self.models.get(b_lang).is_top(b.core.expose()) && !a.in_dict
                && (context.1.is_none() || context.1 == Some(b_lang))
            {
                return fix("dict-short".into());
            }
            return keep("short");
        }
        if !b.valid {
            return keep("b-invalid");
        }
        if b_len <= 3 {
            return keep("short-ngram");
        }
        let b_score = self.models.get(b_lang).score(b.core.expose());
        let a_score = a.valid.then(|| self.models.get(a_lang).score(core));
        let margin = sens.margin * if context == (Some(a_lang), Some(a_lang)) { 2.0 } else { 1.0 };
        if b_score < sens.floor {
            return keep(&format!("ngram-floor {b_score:.2}"));
        }
        let Some(a_score) = a_score else {
            return fix(format!("ngram-only {b_score:.2}"));
        };
        let delta = b_score - a_score;
        if delta >= margin {
            return fix(format!("ngram Δ={delta:.2}"));
        }
        keep(&format!("ngram Δ={delta:.2} < {margin:.2}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::testing::{press, us_ru};

    fn detector() -> Detector {
        Detector {
            keymap: us_ru(),
            models: Models::builtin(),
            exceptions: Exceptions::default(),
            sensitivity: Sensitivity::named("normal"),
        }
    }

    #[test]
    fn rules() {
        let d = detector();
        let fix = |typed: &str, group: usize| {
            let keys = press(&d.keymap, typed, group);
            let dec = d.decide(&keys, group, 1 - group, (None, None));
            (dec.action, dec.target.expose().to_string())
        };
        assert_eq!(fix("ghbdtn", 0), (Action::Fix, "привет".into()));
        assert_eq!(fix("руддщ", 1), (Action::Fix, "hello".into()));
        assert_eq!(fix("hello", 0).0, Action::Keep);
        assert_eq!(fix("привет", 1).0, Action::Keep);
        assert_eq!(fix("ghbdtn1", 0).0, Action::Keep);

        let keys = press(&d.keymap, "ghbd", 0);
        assert_eq!(d.early_word_target(&keys, 0, 1).unwrap().expose(), "прив");
        let keys = press(&d.keymap, "чтото", 1);
        assert_eq!(d.particles_fix(&keys, 1).unwrap().expose(), "что-то");
        let keys = press(&d.keymap, "пирогб", 1);
        assert_eq!(d.punct_fix(&keys, 1, 0), Some(','));
        let keys = press(&d.keymap, "ПРивет", 1);
        assert_eq!(d.two_capitals_fix(&keys, 1).unwrap().expose(), "Привет");
    }

    #[test]
    fn reason_never_contains_text() {
        let d = detector();
        for word in ["ghbdtn", "hello", "qwerty", "gfhjkm"] {
            let keys = press(&d.keymap, word, 0);
            let dec = d.decide(&keys, 0, 1, (None, None));
            assert!(!dec.reason.contains(word));
            assert!(!dec.reason.contains(d.decode(&keys, 1).expose()));
        }
    }
}
