//! Sway: IPC (протокол i3) через $SWAYSOCK.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;
use std::time::Duration;

use serde_json::Value;

use super::{report, spec_for_description, LayoutBackend, Result, SharedState};
use crate::keymap::LayoutSpec;
use crate::Event;

const RUN_COMMAND: u32 = 0;
const SUBSCRIBE: u32 = 2;
const GET_INPUTS: u32 = 100;

fn socket() -> Result<String> {
    std::env::var("SWAYSOCK").map_err(|_| "нет сокета sway (SWAYSOCK)".to_string())
}

fn send(stream: &mut UnixStream, kind: u32, payload: &str) -> std::io::Result<()> {
    let mut msg = b"i3-ipc".to_vec();
    msg.extend((payload.len() as u32).to_le_bytes());
    msg.extend(kind.to_le_bytes());
    msg.extend(payload.as_bytes());
    stream.write_all(&msg)
}

fn recv(stream: &mut UnixStream) -> std::io::Result<(u32, Value)> {
    let mut head = [0u8; 14];
    stream.read_exact(&mut head)?;
    let len = u32::from_le_bytes(head[6..10].try_into().unwrap()) as usize;
    let kind = u32::from_le_bytes(head[10..14].try_into().unwrap());
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    let value = serde_json::from_slice(&body).map_err(std::io::Error::other)?;
    Ok((kind, value))
}

fn request(kind: u32, payload: &str) -> Result<Value> {
    let mut s = UnixStream::connect(socket()?).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(1))).ok();
    send(&mut s, kind, payload).map_err(|e| e.to_string())?;
    recv(&mut s).map(|(_, v)| v).map_err(|e| e.to_string())
}

/// Клавиатура, за которой следим: первая настоящая с двумя и более раскладками.
fn main_keyboard(inputs: &Value) -> Option<&Value> {
    inputs.as_array()?.iter().find(|d| {
        d["type"] == "keyboard"
            && d["name"] != crate::typist::VIRTUAL_NAME
            && d["xkb_layout_names"].as_array().is_some_and(|a| a.len() >= 2)
    })
}

pub struct SwayBackend {
    specs: Vec<LayoutSpec>,
    state: SharedState,
}

fn query() -> Option<usize> {
    let inputs = request(GET_INPUTS, "").ok()?;
    main_keyboard(&inputs)?["xkb_active_layout_index"].as_u64().map(|i| i as usize)
}

impl SwayBackend {
    pub fn new(tx: Sender<Event>) -> Result<Self> {
        let inputs = request(GET_INPUTS, "").map_err(|e| format!("sway не отвечает: {e}"))?;
        let kbd = main_keyboard(&inputs).ok_or("в sway у клавиатуры меньше двух раскладок")?;
        let mut specs = Vec::new();
        for name in kbd["xkb_layout_names"].as_array().unwrap() {
            let name = name.as_str().unwrap_or("");
            specs.push(spec_for_description(name).ok_or(format!("раскладка «{name}» не найдена в базе XKB"))?);
        }
        let backend = SwayBackend { specs, state: SharedState::default() };
        backend.state.lock().unwrap().current = query();
        let state = backend.state.clone();
        let names = kbd["xkb_layout_names"].clone();
        std::thread::Builder::new()
            .name("sway-layout".into())
            .spawn(move || {
                let Ok(mut s) = UnixStream::connect(socket().unwrap_or_default()) else { return };
                if send(&mut s, SUBSCRIBE, r#"["input"]"#).is_err() {
                    return;
                }
                while let Ok((kind, value)) = recv(&mut s) {
                    if kind & 0x8000_0000 != 0 && value["change"] == "xkb_layout" {
                        report(&state, &tx, query());
                    } else if value["change"] == "xkb_keymap"
                        && request(GET_INPUTS, "").ok().as_ref().and_then(main_keyboard).map(|k| &k["xkb_layout_names"]) != Some(&names)
                    {
                        let _ = tx.send(Event::LayoutsChanged);
                    }
                }
            })
            .expect("поток раскладки sway");
        Ok(backend)
    }
}

impl LayoutBackend for SwayBackend {
    fn name(&self) -> &'static str {
        "sway"
    }

    fn layouts(&self) -> Vec<LayoutSpec> {
        self.specs.clone()
    }

    fn xkb_options(&self) -> Vec<String> {
        Vec::new()
    }

    fn current(&self) -> Option<usize> {
        self.state.lock().unwrap().current
    }

    fn set(&self, index: usize) -> Result<()> {
        self.state.lock().unwrap().expected = Some(index);
        let reply = request(RUN_COMMAND, &format!("input type:keyboard xkb_switch_layout {index}"))?;
        if reply[0]["success"] != true {
            self.state.lock().unwrap().expected = None;
            return Err("sway отказал в переключении".into());
        }
        self.state.lock().unwrap().current = Some(index);
        Ok(())
    }

    fn wait_applied(&self, index: usize, settle: Duration) -> bool {
        std::thread::sleep(settle);
        let now = query();
        self.state.lock().unwrap().current = now;
        now == Some(index)
    }

    fn always_switch(&self) -> bool {
        true
    }
}
