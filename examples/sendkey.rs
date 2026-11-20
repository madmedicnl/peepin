//! Send a key chord to the focused X11 window: `cargo run --example sendkey -- ctrl+o`
//! Used to exercise peepin without a keyboard.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, KEY_PRESS_EVENT, KEY_RELEASE_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;

fn main() {
    let args = std::env::args().skip(1);
    let mut wid: Option<u32> = None;
    let mut title: Option<String> = None;
    let mut chord = "ctrl+o".to_string();
    for arg in args {
        if let Some(value) = arg.strip_prefix("--wid=") {
            wid = value.parse().ok();
        } else if let Some(value) = arg.strip_prefix("--title=") {
            title = Some(value.to_string());
        } else {
            chord = arg;
        }
    }
    let (conn, screen) = x11rb::connect(None).expect("connect");
    let root = conn.setup().roots[screen].root;
    let setup = conn.setup();
    let min = setup.min_keycode;
    let max = setup.max_keycode;
    let map = conn
        .get_keyboard_mapping(min, max - min + 1)
        .expect("keymap")
        .reply()
        .expect("keymap reply");
    let find = |name: &str| -> Option<u8> {
        let sym = match name {
            "ctrl" | "control" => 0xffe3,
            "shift" => 0xffe1,
            "alt" => 0xffe9,
            "space" => 0x0020,
            "return" | "enter" => 0xff0d,
            "tab" => 0xff09,
            "escape" | "esc" => 0xff1b,
            "up" => 0xff52,
            "down" => 0xff54,
            "left" => 0xff51,
            "right" => 0xff53,
            other => {
                let ch = other.chars().next()?;
                ch as u32
            }
        };
        for (i, syms) in map
            .keysyms
            .chunks(map.keysyms_per_keycode as usize)
            .enumerate()
        {
            if syms.contains(&sym) {
                return Some(min + i as u8);
            }
        }
        None
    };
    let mut codes = Vec::new();
    for part in chord.split('+') {
        match find(part.trim()) {
            Some(code) => codes.push(code),
            None => {
                eprintln!("no keycode for {part}");
                std::process::exit(2);
            }
        }
    }
    if let Some(want) = title {
        fn find(conn: &impl Connection, window: u32, want: &str, depth: usize) -> Option<u32> {
            if depth > 4 {
                return None;
            }
            let tree = conn.query_tree(window).ok()?.reply().ok()?;
            for child in tree.children {
                let name = conn
                    .get_property(
                        false,
                        child,
                        x11rb::protocol::xproto::AtomEnum::WM_NAME,
                        x11rb::protocol::xproto::AtomEnum::STRING,
                        0,
                        64,
                    )
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| String::from_utf8_lossy(&r.value).into_owned())
                    .unwrap_or_default();
                if name == want {
                    return Some(child);
                }
                if let Some(found) = find(conn, child, want, depth + 1) {
                    return Some(found);
                }
            }
            None
        }
        match find(&conn, root, &want, 0) {
            Some(id) => {
                println!("window {want} = {id}");
                wid = Some(id);
            }
            None => {
                eprintln!("no window titled {want}");
                std::process::exit(2);
            }
        }
    }
    if let Some(wid) = wid {
        use x11rb::protocol::xproto::InputFocus;
        conn.set_input_focus(InputFocus::PARENT, wid, x11rb::CURRENT_TIME)
            .ok();
        conn.flush().ok();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let fake = |kind: u8, code: u8| {
        conn.xtest_fake_input(kind, code, 0, root, 0, 0, 0).ok();
        conn.flush().ok();
    };
    for code in &codes {
        fake(KEY_PRESS_EVENT, *code);
    }
    for code in codes.iter().rev() {
        fake(KEY_RELEASE_EVENT, *code);
    }
    println!("sent {chord}");
}
