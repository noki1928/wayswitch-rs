//! Жест ручного исправления: двойной (слово) и тройной (фраза) Shift.
//!
//! Срабатывает на отпускании, чтобы синтетические символы не ушли с зажатым
//! физическим Shift. Любая другая клавиша между нажатиями разрывает серию.

use crate::keycodes as kc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    Word,
    Phrase,
}

pub struct ShiftGesture {
    window: f64,
    count: u32,
    last_press: Option<f64>,
    broken: bool,
}

impl ShiftGesture {
    pub fn new() -> Self {
        ShiftGesture { window: 0.4, count: 0, last_press: None, broken: false }
    }

    pub fn reset(&mut self) {
        self.count = 0;
        self.last_press = None;
        self.broken = false;
    }

    pub fn on_key(&mut self, code: u16, value: i32, now: f64) -> Option<Gesture> {
        if kc::is_shift(code) {
            match value {
                1 => {
                    self.count = match self.last_press {
                        Some(t) if now - t <= self.window => self.count + 1,
                        _ => 1,
                    };
                    self.last_press = Some(now);
                    self.broken = false;
                }
                0 if self.broken => self.reset(),
                0 if self.count == 2 => return Some(Gesture::Word),
                0 if self.count >= 3 => {
                    self.reset();
                    return Some(Gesture::Phrase);
                }
                _ => {}
            }
            return None;
        }
        if value == 1 {
            self.count = 0;
            self.last_press = None;
            self.broken = true;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_and_triple() {
        let mut g = ShiftGesture::new();
        let tap = |g: &mut ShiftGesture, t: f64| {
            g.on_key(kc::KEY_LEFTSHIFT, 1, t);
            g.on_key(kc::KEY_LEFTSHIFT, 0, t + 0.05)
        };
        assert_eq!(tap(&mut g, 0.0), None);
        assert_eq!(tap(&mut g, 0.2), Some(Gesture::Word));
        assert_eq!(tap(&mut g, 0.4), Some(Gesture::Phrase));
        g.on_key(30, 1, 1.0);
        assert_eq!(tap(&mut g, 1.1), None);
    }
}
