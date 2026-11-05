//! Коды клавиш Linux (input-event-codes.h), нужные демону.

pub const KEY_ESC: u16 = 1;
pub const KEY_1: u16 = 2;
pub const KEY_0: u16 = 11;
pub const KEY_MINUS: u16 = 12;
pub const KEY_EQUAL: u16 = 13;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_SLASH: u16 = 53;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_F1: u16 = 59;
pub const KEY_F10: u16 = 68;
pub const KEY_NUMLOCK: u16 = 69;
pub const KEY_SCROLLLOCK: u16 = 70;
pub const KEY_F11: u16 = 87;
pub const KEY_F12: u16 = 88;
pub const KEY_KPENTER: u16 = 96;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_HOME: u16 = 102;
pub const KEY_UP: u16 = 103;
pub const KEY_PAGEUP: u16 = 104;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_END: u16 = 107;
pub const KEY_DOWN: u16 = 108;
pub const KEY_PAGEDOWN: u16 = 109;
pub const KEY_INSERT: u16 = 110;
pub const KEY_DELETE: u16 = 111;
pub const KEY_PAUSE: u16 = 119;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;
pub const KEY_F13: u16 = 183;
pub const KEY_F24: u16 = 194;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_TASK: u16 = 0x117;
pub const BTN_TOUCH: u16 = 0x14a;

pub const REL_HWHEEL: u16 = 6;
pub const REL_WHEEL: u16 = 8;
pub const REL_WHEEL_HI_RES: u16 = 11;
pub const REL_HWHEEL_HI_RES: u16 = 12;

pub const ALL_MODIFIERS: [u16; 8] = [
    KEY_LEFTSHIFT, KEY_RIGHTSHIFT, KEY_LEFTCTRL, KEY_RIGHTCTRL,
    KEY_LEFTALT, KEY_RIGHTALT, KEY_LEFTMETA, KEY_RIGHTMETA,
];

pub fn is_shift(code: u16) -> bool {
    code == KEY_LEFTSHIFT || code == KEY_RIGHTSHIFT
}

pub fn is_meta(code: u16) -> bool {
    code == KEY_LEFTMETA || code == KEY_RIGHTMETA
}

/// Ctrl, Alt, Super: с ними нажатие — команда, а не текст.
pub fn is_command_modifier(code: u16) -> bool {
    matches!(code, KEY_LEFTCTRL | KEY_RIGHTCTRL | KEY_LEFTALT | KEY_RIGHTALT) || is_meta(code)
}

pub fn is_modifier(code: u16) -> bool {
    is_shift(code) || is_command_modifier(code)
}

pub fn is_lock(code: u16) -> bool {
    matches!(code, KEY_CAPSLOCK | KEY_NUMLOCK | KEY_SCROLLLOCK | KEY_PAUSE)
}

/// Правка текста руками: после неё автоисправление на время замирает.
pub fn is_edit(code: u16) -> bool {
    matches!(
        code,
        KEY_BACKSPACE | KEY_DELETE | KEY_HOME | KEY_END | KEY_PAGEUP | KEY_PAGEDOWN
            | KEY_LEFT | KEY_RIGHT | KEY_UP | KEY_DOWN
    )
}

/// Клавиши, после которых каретка могла уйти: слово забываем.
pub fn is_reset(code: u16) -> bool {
    matches!(
        code,
        KEY_ENTER | KEY_KPENTER | KEY_TAB | KEY_ESC | KEY_UP | KEY_DOWN | KEY_LEFT | KEY_RIGHT
            | KEY_HOME | KEY_END | KEY_PAGEUP | KEY_PAGEDOWN | KEY_DELETE | KEY_INSERT
            | KEY_F11 | KEY_F12
    ) || (KEY_F1..=KEY_F10).contains(&code)
        || (KEY_F13..=KEY_F24).contains(&code)
}

/// Не буквы, но часть слова: цифры, «-», «=», «/».
pub fn is_word_extra(code: u16) -> bool {
    (KEY_1..=KEY_0).contains(&code) || matches!(code, KEY_MINUS | KEY_EQUAL | KEY_SLASH)
}

/// Кнопка мыши, тачпада или касание экрана.
pub fn is_pointer_button(code: u16) -> bool {
    (BTN_LEFT..=BTN_TASK).contains(&code) || code == BTN_TOUCH
}

pub fn is_wheel(code: u16) -> bool {
    matches!(code, REL_HWHEEL | REL_WHEEL | REL_WHEEL_HI_RES | REL_HWHEEL_HI_RES)
}
