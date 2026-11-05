//! Раскладки: «код клавиши → символ» по группам xkb (через libxkbcommon).

use std::collections::{HashMap, HashSet};

use crate::secure::SecretText;

pub const CYRILLIC: &str = "абвгдеёжзийклмнопрстуфхцчшщъыьэюя";
pub const LATIN: &str = "abcdefghijklmnopqrstuvwxyz";

/// KEY_A..KEY_L: по ним определяем алфавит группы.
const ALPHABET_PROBE: [u16; 9] = [30, 31, 32, 33, 34, 35, 36, 37, 38];
/// Цифровой блок: те же символы, что у основных клавиш, но печатать ими нельзя.
const NUMPAD_KEYS: [u16; 21] =
    [55, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 96, 98, 117, 118, 121, 179, 180];
const MAX_CODE: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutSpec {
    pub layout: String,
    pub variant: String,
}

impl LayoutSpec {
    pub fn new(layout: &str, variant: &str) -> Self {
        LayoutSpec { layout: layout.into(), variant: variant.into() }
    }

    /// 'us+dvorak' (формат gsettings) → LayoutSpec.
    pub fn parse(id: &str) -> Self {
        let (layout, variant) = id.split_once('+').unwrap_or((id, ""));
        LayoutSpec::new(layout, variant)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alphabet {
    Russian,
    Latin,
    Other,
}

/// Одно нажатие из буфера: код и состояние Shift/Caps Lock в момент нажатия.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyPress {
    pub code: u16,
    pub shift: bool,
    pub caps: bool,
}

impl zeroize::Zeroize for KeyPress {
    fn zeroize(&mut self) {
        // zeroize пишет через volatile: компилятор не выкинет затирание.
        self.code.zeroize();
        self.shift.zeroize();
        self.caps.zeroize();
    }
}

pub const SPACE: KeyPress = KeyPress { code: crate::keycodes::KEY_SPACE, shift: false, caps: false };

type Table = Vec<(Option<char>, Option<char>)>;

pub struct Keymap {
    tables: Vec<Table>,
    reverse: Vec<HashMap<char, (u16, bool)>>,
    letter_keys: HashSet<u16>,
}

impl Keymap {
    pub fn from_tables(tables: Vec<Table>) -> Self {
        let reverse = tables.iter().map(build_reverse).collect();
        let mut letter_keys = HashSet::new();
        for table in &tables {
            for (code, (lo, hi)) in table.iter().enumerate() {
                if lo.is_some_and(char::is_alphabetic) || hi.is_some_and(char::is_alphabetic) {
                    letter_keys.insert(code as u16);
                }
            }
        }
        Keymap { tables, reverse, letter_keys }
    }

    /// Собрать раскладку libxkbcommon из имён xkb (как это делает композитор).
    pub fn from_names(specs: &[LayoutSpec], options: &[String]) -> Result<Self, String> {
        use xkbcommon::xkb;

        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let layouts = specs.iter().map(|s| s.layout.as_str()).collect::<Vec<_>>().join(",");
        let variants = specs.iter().map(|s| s.variant.as_str()).collect::<Vec<_>>().join(",");
        let keymap = xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            &layouts,
            &variants,
            Some(options.join(",")),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| format!("xkb: не удалось собрать раскладку {layouts:?}"))?;
        let shift_mask = 1u32 << keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
        let mut tables = Vec::new();
        for group in 0..keymap.num_layouts() {
            let mut table: Table = vec![(None, None); MAX_CODE];
            for shift in [false, true] {
                let mut state = xkb::State::new(&keymap);
                state.update_mask(if shift { shift_mask } else { 0 }, 0, 0, 0, 0, group);
                for (code, slot) in table.iter_mut().enumerate().skip(1) {
                    let s = state.key_get_utf8(xkb::Keycode::new(code as u32 + 8));
                    let mut chars = s.chars();
                    let ch = match (chars.next(), chars.next()) {
                        (Some(c), None) if !c.is_control() => Some(c),
                        _ => None,
                    };
                    if shift {
                        slot.1 = ch;
                    } else {
                        slot.0 = ch;
                    }
                }
            }
            tables.push(table);
        }
        Ok(Keymap::from_tables(tables))
    }

    pub fn num_groups(&self) -> usize {
        self.tables.len()
    }

    pub fn char(&self, code: u16, group: usize, shift: bool, caps: bool) -> Option<char> {
        let (lo, hi) = *self.tables.get(group)?.get(code as usize)?;
        let ch = if shift { hi } else { lo }?;
        // Caps Lock меняет регистр только у букв.
        if caps && ch.is_alphabetic() {
            Some(swap_case(ch))
        } else {
            Some(ch)
        }
    }

    pub fn key_for(&self, ch: char, group: usize) -> Option<(u16, bool)> {
        self.reverse.get(group)?.get(&ch).copied()
    }

    /// Клавиша даёт букву хотя бы в одной группе (так `;` попадает в слово: в ru это «ж»).
    pub fn is_letter_key(&self, code: u16) -> bool {
        self.letter_keys.contains(&code)
    }

    pub fn alphabet(&self, group: usize) -> Alphabet {
        let chars: Vec<char> = ALPHABET_PROBE
            .iter()
            .filter_map(|&c| self.char(c, group, false, false))
            .flat_map(char::to_lowercase)
            .collect();
        if chars.is_empty() {
            Alphabet::Other
        } else if chars.iter().all(|c| CYRILLIC.contains(*c)) {
            Alphabet::Russian
        } else if chars.iter().all(|c| LATIN.contains(*c)) {
            Alphabet::Latin
        } else {
            Alphabet::Other
        }
    }

    pub fn decode(&self, keys: &[KeyPress], group: usize) -> SecretText {
        let mut s = String::with_capacity(keys.len() * 2);
        for k in keys {
            if let Some(c) = self.char(k.code, group, k.shift, k.caps) {
                s.push(c);
            }
        }
        SecretText::new(s)
    }

    /// Символы → (код, Shift) в группе; при Caps Lock Shift у букв инвертируется.
    pub fn keys_for_typing(&self, text: &str, group: usize, caps_on: bool) -> Option<Vec<(u16, bool)>> {
        text.chars()
            .map(|ch| {
                let (code, shift) = self.key_for(ch, group)?;
                Some((code, if caps_on && ch.is_alphabetic() { !shift } else { shift }))
            })
            .collect()
    }

    /// Нажатия, которыми текст набирается в группе (для буфера после исправления).
    pub fn presses_for(&self, text: &str, group: usize) -> Option<Vec<KeyPress>> {
        text.chars()
            .map(|ch| self.key_for(ch, group).map(|(code, shift)| KeyPress { code, shift, caps: false }))
            .collect()
    }
}

fn build_reverse(table: &Table) -> HashMap<char, (u16, bool)> {
    let mut rev: HashMap<char, (u16, bool)> = HashMap::new();
    // Сначала без Shift, затем с Shift — без Shift перекрывает. Основная
    // клавиша важнее цифрового блока («.» есть и на KEY_KPDOT).
    for shift in [false, true] {
        for (code, (lo, hi)) in table.iter().enumerate() {
            let Some(ch) = (if shift { *hi } else { *lo }) else { continue };
            let code = code as u16;
            if let Some(&(prev, _)) = rev.get(&ch) {
                let replace = !NUMPAD_KEYS.contains(&code) && NUMPAD_KEYS.contains(&prev);
                if !replace {
                    continue;
                }
            }
            rev.insert(ch, (code, shift));
        }
    }
    rev
}

pub fn swap_case(c: char) -> char {
    if c.is_uppercase() {
        c.to_lowercase().next().unwrap_or(c)
    } else {
        c.to_uppercase().next().unwrap_or(c)
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// us и ru, только буквы и пара знаков — для тестов без libxkbcommon.
    pub fn us_ru() -> Keymap {
        let rows: [(u16, char, char); 37] = [
            (16, 'q', 'й'), (17, 'w', 'ц'), (18, 'e', 'у'), (19, 'r', 'к'), (20, 't', 'е'),
            (21, 'y', 'н'), (22, 'u', 'г'), (23, 'i', 'ш'), (24, 'o', 'щ'), (25, 'p', 'з'),
            (26, '[', 'х'), (27, ']', 'ъ'), (30, 'a', 'ф'), (31, 's', 'ы'), (32, 'd', 'в'),
            (33, 'f', 'а'), (34, 'g', 'п'), (35, 'h', 'р'), (36, 'j', 'о'), (37, 'k', 'л'),
            (38, 'l', 'д'), (39, ';', 'ж'), (40, '\'', 'э'), (44, 'z', 'я'), (45, 'x', 'ч'),
            (46, 'c', 'с'), (47, 'v', 'м'), (48, 'b', 'и'), (49, 'n', 'т'), (50, 'm', 'ь'),
            (51, ',', 'б'), (52, '.', 'ю'), (53, '/', '.'), (57, ' ', ' '), (12, '-', '-'),
            (2, '1', '1'), (41, '`', 'ё'),
        ];
        let mut us: Table = vec![(None, None); MAX_CODE];
        let mut ru: Table = vec![(None, None); MAX_CODE];
        for (code, u, r) in rows {
            let up = |c: char| if c.is_alphabetic() { swap_case(c) } else { c };
            us[code as usize] = (Some(u), Some(up(u)));
            ru[code as usize] = (Some(r), Some(up(r)));
        }
        ru[53].1 = Some(',');
        Keymap::from_tables(vec![us, ru])
    }

    pub fn press(km: &Keymap, text: &str, group: usize) -> Vec<KeyPress> {
        km.presses_for(text, group).expect("символ есть в раскладке")
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn decode_other_group() {
        let km = us_ru();
        let keys = press(&km, "ghbdtn", 0);
        assert_eq!(km.decode(&keys, 1).expose(), "привет");
        assert_eq!(km.alphabet(0), Alphabet::Latin);
        assert_eq!(km.alphabet(1), Alphabet::Russian);
    }
}
