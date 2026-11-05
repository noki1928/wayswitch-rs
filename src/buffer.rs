//! Буфер набранных нажатий.
//!
//! Хранятся коды клавиш с Shift/Caps Lock, а не символы: какой символ
//! получился, зависит от раскладки. Сам буфер — [`LockedBuf`]: закреплён в
//! ОЗУ и затирается при сбросе; копии наружу — `Zeroizing<Vec<_>>`.

use std::collections::HashSet;

use zeroize::Zeroizing;

use crate::keycodes as kc;
use crate::keymap::{KeyPress, Keymap, SPACE};
use crate::secure::LockedBuf;

pub type Keys = Zeroizing<Vec<KeyPress>>;
const MAX_KEYS: usize = 512;

pub enum BufferEvent {
    /// Слово завершено пробелом (или Tab/Enter, если включено).
    Word(Keys),
    /// Слово растёт.
    Letter(Keys),
    Reset,
}

pub struct InputBuffer {
    keys: LockedBuf<KeyPress, MAX_KEYS>,
    held: HashSet<(u32, u16)>,
    pub caps: bool,
    pub phrase_timeout: f64,
    terminators: HashSet<u16>,
    last_time: Option<f64>,
}

fn keys(slice: &[KeyPress]) -> Keys {
    Zeroizing::new(slice.to_vec())
}

impl InputBuffer {
    pub fn new(phrase_timeout: f64, terminators: HashSet<u16>) -> Self {
        InputBuffer {
            keys: LockedBuf::new(),
            held: HashSet::new(),
            caps: false,
            phrase_timeout,
            terminators,
            last_time: None,
        }
    }

    pub fn set_terminators(&mut self, t: HashSet<u16>) {
        self.terminators = t;
    }

    fn shift_held(&self) -> bool {
        self.held.iter().any(|&(_, c)| kc::is_shift(c))
    }

    fn command_held(&self) -> bool {
        self.held.iter().any(|&(_, c)| kc::is_command_modifier(c))
    }

    /// Нажатие или отпускание, пришедшее во время печати: в буфер не идёт.
    pub fn note(&mut self, code: u16, value: i32, device: u32) {
        match value {
            0 => {
                self.held.remove(&(device, code));
            }
            1 => {
                self.held.insert((device, code));
            }
            _ => {}
        }
    }

    pub fn device_gone(&mut self, device: u32) {
        self.held.retain(|&(d, _)| d != device);
        self.reset();
    }

    /// Сбросить фразу, если с последнего нажатия прошло больше phrase_timeout.
    pub fn expire(&mut self, now: f64) -> bool {
        if !self.keys.is_empty() && self.last_time.is_some_and(|t| now - t > self.phrase_timeout) {
            self.keys.clear();
            return true;
        }
        false
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn reset(&mut self) -> bool {
        let had = !self.keys.is_empty();
        self.keys.clear();
        had
    }

    fn reset_event(&mut self) -> Option<BufferEvent> {
        self.reset().then_some(BufferEvent::Reset)
    }

    fn is_word_key(keymap: &Keymap, code: u16) -> bool {
        keymap.is_letter_key(code) || kc::is_word_extra(code)
    }

    pub fn feed(&mut self, keymap: &Keymap, code: u16, value: i32, device: u32, now: f64) -> Option<BufferEvent> {
        let timed_out = self.expire(now);
        self.last_time = Some(now);
        if value == 0 {
            self.held.remove(&(device, code));
            return timed_out.then_some(BufferEvent::Reset);
        }
        if value == 2 {
            // Автоповтор: сколько символов выдаст композитор — неизвестно.
            let ev = if Self::is_word_key(keymap, code) || code == kc::KEY_SPACE || code == kc::KEY_BACKSPACE {
                self.reset_event()
            } else {
                None
            };
            return ev.or(timed_out.then_some(BufferEvent::Reset));
        }
        self.held.insert((device, code));
        let ev = if kc::is_meta(code) {
            self.reset_event()
        } else if kc::is_modifier(code) || code == kc::KEY_CAPSLOCK {
            None
        } else if self.command_held() {
            self.reset_event() // Ctrl+X, Alt+Tab и т. п.
        } else if self.terminators.contains(&code) && !self.keys.is_empty() {
            let word = self.current_word();
            self.reset();
            (!word.is_empty()).then_some(BufferEvent::Word(word))
        } else if kc::is_reset(code) {
            self.reset_event()
        } else if code == kc::KEY_BACKSPACE {
            if self.keys.pop().is_some() {
                None
            } else {
                self.reset_event()
            }
        } else if code == kc::KEY_SPACE {
            let word = self.current_word();
            self.keys.push(SPACE);
            (!word.is_empty()).then_some(BufferEvent::Word(word))
        } else if Self::is_word_key(keymap, code) {
            self.keys.push(KeyPress { code, shift: self.shift_held(), caps: self.caps });
            Some(BufferEvent::Letter(self.current_word()))
        } else {
            self.reset_event()
        };
        match ev {
            None if timed_out => Some(BufferEvent::Reset),
            ev => ev,
        }
    }

    fn word_start(&self, end: usize) -> usize {
        let s = self.keys.as_slice();
        let mut i = end;
        while i > 0 && s[i - 1].code != kc::KEY_SPACE {
            i -= 1;
        }
        i
    }

    pub fn current_word(&self) -> Keys {
        let end = self.keys.len();
        keys(&self.keys.as_slice()[self.word_start(end)..end])
    }

    /// Последнее слово и число пробелов после него.
    pub fn last_word_with_tail(&self) -> (Keys, usize) {
        let s = self.keys.as_slice();
        let mut end = s.len();
        while end > 0 && s[end - 1].code == kc::KEY_SPACE {
            end -= 1;
        }
        let start = self.word_start(end);
        (keys(&s[start..end]), s.len() - end)
    }

    pub fn phrase(&self) -> Keys {
        keys(self.keys.as_slice())
    }

    /// Заменить последние `count` нажатий.
    pub fn replace_tail(&mut self, count: usize, new: &[KeyPress]) {
        let len = self.keys.len();
        self.keys.truncate(len - count.min(len));
        self.keys.extend_from_slice(new);
    }

    /// Кончается ли фраза нажатиями `keys`, за которыми только пробелы.
    /// Возвращает число этих пробелов.
    pub fn ends_with_before_spaces(&self, keys: &[KeyPress]) -> Option<usize> {
        let s = self.keys.as_slice();
        let mut end = s.len();
        while end > 0 && s[end - 1].code == kc::KEY_SPACE {
            end -= 1;
        }
        (!keys.is_empty() && s[..end].ends_with(keys)).then_some(s.len() - end)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.keys.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::testing::us_ru;

    #[test]
    fn word_on_space_and_backspace() {
        let km = us_ru();
        let mut b = InputBuffer::new(8.0, HashSet::new());
        for code in [34u16, 35, 48] {
            b.feed(&km, code, 1, 1, 0.0);
            b.feed(&km, code, 0, 1, 0.0);
        }
        b.feed(&km, kc::KEY_BACKSPACE, 1, 1, 0.0);
        match b.feed(&km, kc::KEY_SPACE, 1, 1, 0.1) {
            Some(BufferEvent::Word(w)) => assert_eq!(km.decode(&w, 0).expose(), "gh"),
            _ => panic!("ждали слово"),
        }
        assert!(matches!(b.feed(&km, 30, 1, 1, 100.0), Some(BufferEvent::Letter(_))));
        assert_eq!(b.len(), 1, "фраза сброшена по таймауту");
    }
}
