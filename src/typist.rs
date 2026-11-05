//! Печать через виртуальную клавиатуру uinput.

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, EventType, InputEvent, Key};

pub const VIRTUAL_NAME: &str = "WaySwitch-rs Virtual Keyboard";

pub struct UInputTypist {
    dev: VirtualDevice,
}

impl UInputTypist {
    pub fn new() -> std::io::Result<Self> {
        let mut keys = AttributeSet::<Key>::new();
        for code in 1..128u16 {
            keys.insert(Key::new(code));
        }
        // Сразу после загрузки udev может ещё не выставить группу на
        // /dev/uinput: несколько секунд пробуем снова.
        let mut attempt = 0;
        loop {
            let made = crate::secure::with_input_group(|| {
                VirtualDeviceBuilder::new()?.name(VIRTUAL_NAME).with_keys(&keys)?.build()
            });
            match made {
                Ok(dev) => return Ok(UInputTypist { dev }),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < 20 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Нажать/отпустить (value 1/0); SYN добавляется автоматически.
    pub fn key(&mut self, code: u16, value: i32) -> std::io::Result<()> {
        self.dev.emit(&[InputEvent::new(EventType::KEY, code, value)])
    }
}
