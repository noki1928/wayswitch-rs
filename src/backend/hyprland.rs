//! Hyprland: `.socket.sock` для запросов, `.socket2.sock` для событий.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use serde_json::Value;

use super::{parse_layout_list, report, spec_for_description, LayoutBackend, Result, SharedState};
use crate::keymap::LayoutSpec;
use crate::Event;

fn dir() -> Result<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").map_err(|_| "нет HYPRLAND_INSTANCE_SIGNATURE")?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let new = PathBuf::from(runtime).join("hypr").join(&sig);
    if new.join(".socket.sock").exists() {
        return Ok(new);
    }
    Ok(PathBuf::from("/tmp/hypr").join(sig))
}

fn request(cmd: &str) -> Result<String> {
    let mut s = UnixStream::connect(dir()?.join(".socket.sock")).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(1))).ok();
    s.write_all(cmd.as_bytes()).map_err(|e| e.to_string())?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn json(cmd: &str) -> Result<Value> {
    serde_json::from_str(&request(&format!("j/{cmd}"))?).map_err(|e| e.to_string())
}

fn option(name: &str) -> String {
    json(&format!("getoption {name}"))
        .ok()
        .and_then(|v| v["str"].as_str().map(|s| s.trim().trim_matches('"').to_string()))
        .filter(|s| s != "[[EMPTY]]")
        .unwrap_or_default()
}

fn main_keyboard(devices: &Value) -> Option<&Value> {
    let kbs: Vec<&Value> = devices["keyboards"]
        .as_array()?
        .iter()
        .filter(|k| k["name"].as_str().is_some_and(|n| n.replace('-', " ") != crate::typist::VIRTUAL_NAME.to_lowercase()))
        .collect();
    kbs.iter()
        .find(|k| k["main"] == true)
        .or_else(|| kbs.iter().find(|k| k["layout"].as_str().is_some_and(|l| l.contains(','))))
        .copied()
}

pub struct HyprlandBackend {
    specs: Vec<LayoutSpec>,
    options: Vec<String>,
    state: SharedState,
}

fn index_of(specs: &[LayoutSpec], desc: &str) -> Option<usize> {
    let spec = spec_for_description(desc)?;
    specs.iter().position(|s| *s == spec).or_else(|| specs.iter().position(|s| s.layout == spec.layout))
}

fn query(specs: &[LayoutSpec]) -> Option<usize> {
    let devices = json("devices").ok()?;
    let kb = main_keyboard(&devices)?;
    if let Some(i) = kb["active_layout_index"].as_u64() {
        return Some(i as usize);
    }
    index_of(specs, kb["active_keymap"].as_str()?)
}

impl HyprlandBackend {
    pub fn new(tx: Sender<Event>) -> Result<Self> {
        let specs = parse_layout_list(&option("input:kb_layout"), &option("input:kb_variant"));
        if specs.len() < 2 {
            return Err("в Hyprland меньше двух раскладок (input:kb_layout)".into());
        }
        let options = option("input:kb_options").split(',').filter(|s| !s.is_empty()).map(str::to_string).collect();
        let backend = HyprlandBackend { specs, options, state: SharedState::default() };
        backend.state.lock().unwrap().current = query(&backend.specs);
        let (state, specs) = (backend.state.clone(), backend.specs.clone());
        std::thread::Builder::new()
            .name("hypr-layout".into())
            .spawn(move || {
                let Ok(path) = dir() else { return };
                let Ok(s) = UnixStream::connect(path.join(".socket2.sock")) else { return };
                for line in BufReader::new(s).lines().map_while(std::result::Result::ok) {
                    if line.starts_with("activelayout>>") {
                        report(&state, &tx, query(&specs));
                    } else if line.starts_with("configreloaded>>")
                        && parse_layout_list(&option("input:kb_layout"), &option("input:kb_variant")) != specs
                    {
                        let _ = tx.send(Event::LayoutsChanged);
                    }
                }
            })
            .expect("поток раскладки Hyprland");
        Ok(backend)
    }
}

impl LayoutBackend for HyprlandBackend {
    fn name(&self) -> &'static str {
        "hyprland"
    }

    fn layouts(&self) -> Vec<LayoutSpec> {
        self.specs.clone()
    }

    fn xkb_options(&self) -> Vec<String> {
        self.options.clone()
    }

    fn current(&self) -> Option<usize> {
        self.state.lock().unwrap().current
    }

    fn set(&self, index: usize) -> Result<()> {
        self.state.lock().unwrap().expected = Some(index);
        let reply = request(&format!("switchxkblayout all {index}"))?;
        if reply.trim() != "ok" && query(&self.specs) != Some(index) {
            self.state.lock().unwrap().expected = None;
            return Err("Hyprland отказал в переключении".into());
        }
        self.state.lock().unwrap().current = Some(index);
        Ok(())
    }

    fn wait_applied(&self, index: usize, settle: Duration) -> bool {
        std::thread::sleep(settle);
        let now = query(&self.specs);
        self.state.lock().unwrap().current = now;
        now == Some(index)
    }

    fn always_switch(&self) -> bool {
        true
    }
}
