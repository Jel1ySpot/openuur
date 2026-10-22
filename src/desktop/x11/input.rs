//! XTest input thread.
//!
//! `InputSink` methods only enqueue a command; one thread owns the XTest
//! connection, the keyboard map and the wheel accumulator. Keys are looked up
//! against the server's live mapping, shifted when the matching keysym lives on
//! a shifted level, and characters without a keycode are typed through a
//! temporarily remapped spare keycode. Auto-repeat is disabled while a
//! synthetic key is held so the server does not repeat it behind the client's
//! back.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::sync::watch;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, Keycode};
use x11rb::protocol::xtest;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

use super::super::input_map::{keysym_for, windows_vk_to_keysym};
use super::super::{DesktopState, InputSink};

const INIT_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_WAIT: Duration = Duration::from_millis(50);
/// One notch is 120 units, matching the Windows wheel delta.
const NOTCH: f64 = 120.0;

/// The spare keycode is server-global, so every input thread (one per desktop
/// peer) serializes the operations that remap it.
static KEYBOARD_LOCK: Mutex<()> = Mutex::new(());

fn keyboard_lock() -> std::sync::MutexGuard<'static, ()> {
    KEYBOARD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) async fn start(
    display: Option<String>,
    state: Arc<DesktopState>,
    stop: Arc<AtomicBool>,
    locked: watch::Receiver<Option<bool>>,
) -> Result<Arc<dyn InputSink>, String> {
    let (commands, receiver) = mpsc::sync_channel(1024);
    let (ready_tx, ready_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("openuur-x11-input".into())
        .spawn(move || run(display.as_deref(), state, stop, locked, receiver, ready_tx))
        .map_err(|error| format!("cannot start the X11 input thread: {error}"))?;
    match tokio::time::timeout(INIT_TIMEOUT, ready_rx).await {
        Ok(Ok(Ok(()))) => Ok(Arc::new(Input { commands })),
        Ok(Ok(Err(error))) => Err(error),
        Ok(Err(_)) => Err("X11 input thread exited during startup".to_owned()),
        Err(_) => Err("X11 input startup timed out".to_owned()),
    }
}

enum Command {
    Absolute(f64, f64),
    Relative(i32, i32),
    Button(i32, bool),
    Scroll(i32, i32),
    Key(i32, bool),
    Text(String),
}

struct Input {
    commands: mpsc::SyncSender<Command>,
}

impl Input {
    /// Pointer motion is coalescible, so a saturated queue drops it. Key and
    /// button transitions must not be lost; they wait briefly for the X
    /// server to drain.
    fn send(&self, command: Command, reliable: bool) {
        match self.commands.try_send(command) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(command)) if reliable => {
                std::thread::sleep(Duration::from_millis(1));
                let _ = self.commands.try_send(command);
            }
            Err(_) => {}
        }
    }
}

impl InputSink for Input {
    fn pointer_absolute(&self, x: f64, y: f64) {
        self.send(Command::Absolute(x, y), false);
    }

    fn pointer_relative(&self, dx: i32, dy: i32) {
        self.send(Command::Relative(dx, dy), false);
    }

    fn button(&self, button: i32, pressed: bool) {
        self.send(Command::Button(button, pressed), true);
    }

    fn scroll(&self, dx: i32, dy: i32) {
        self.send(Command::Scroll(dx, dy), false);
    }

    fn key(&self, vk: i32, pressed: bool) {
        self.send(Command::Key(vk, pressed), true);
    }

    fn text(&self, text: &str) {
        self.send(Command::Text(text.to_owned()), true);
    }
}

fn run(
    display: Option<&str>,
    state: Arc<DesktopState>,
    stop: Arc<AtomicBool>,
    locked: watch::Receiver<Option<bool>>,
    commands: mpsc::Receiver<Command>,
    ready: oneshot::Sender<Result<(), String>>,
) {
    let (connection, screen_num) = match x11rb::connect(display) {
        Ok(connected) => connected,
        Err(error) => {
            let _ = ready.send(Err(format!("X11 input connection: {error}")));
            return;
        }
    };
    let root = connection.setup().roots[screen_num].root;
    let mut keyboard = match Keyboard::load(&connection) {
        Ok(keyboard) => keyboard,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    let power = super::super::session_power::RemoteSessionPower::subscribe();
    let mut wheel = Wheel::default();
    let mut held: HashMap<i32, Held> = HashMap::new();
    let mut buttons: HashSet<u8> = HashSet::new();
    let mut paused = false;

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        {
            let _guard = keyboard_lock();
            drain_mapping_events(&connection, &mut keyboard);
        }
        let sleeping = *power.borrow();
        let locked_now = super::lock_active(&locked);
        if sleeping || locked_now {
            if !paused {
                let _guard = keyboard_lock();
                release_all(&connection, &mut keyboard, &mut held, &mut buttons);
                paused = true;
            }
            // Keep draining the command channel so it never backs up.
            match commands.recv_timeout(COMMAND_WAIT) {
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        paused = false;

        let command = match commands.recv_timeout(COMMAND_WAIT) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let result = {
            let _guard = keyboard_lock();
            match command {
                Command::Absolute(x, y) => absolute(&connection, root, &state, x, y),
                Command::Relative(dx, dy) => relative(&connection, dx, dy),
                Command::Button(wire, pressed) => button(&connection, wire, pressed, &mut buttons),
                Command::Scroll(dx, dy) => wheel.scroll(&connection, dx, dy),
                Command::Key(vk, pressed) => {
                    keyboard.key(&connection, root, vk, pressed, &mut held)
                }
                Command::Text(text) => {
                    // The viewer may send CRLF; a terminal should see one
                    // Return per line.
                    let text = text.replace("\r\n", "\n");
                    let mut failure = Ok(());
                    for ch in text.chars() {
                        // A long paste must not keep typing after the screen
                        // locks, the machine suspends, or the session ends.
                        if stop.load(Ordering::Acquire)
                            || *power.borrow()
                            || super::lock_active(&locked)
                        {
                            break;
                        }
                        let Some(keysym) = keysym_for(ch) else {
                            continue;
                        };
                        if let Err(error) = keyboard.type_keysym(&connection, root, keysym) {
                            failure = Err(error);
                            break;
                        }
                    }
                    failure
                }
            }
        };
        if let Err(error) = result {
            tracing::warn!("X11 input failed: {error}");
        }
        let _ = connection.flush();
    }

    {
        let _guard = keyboard_lock();
        release_all(&connection, &mut keyboard, &mut held, &mut buttons);
        keyboard.restore(&connection);
    }
    let _ = connection.flush();
}

fn drain_mapping_events(connection: &impl Connection, keyboard: &mut Keyboard) {
    while let Ok(Some(event)) = connection.poll_for_event() {
        if let Event::MappingNotify(event) = event {
            // Modifier changes move Shift and invalidate the spare keycode's
            // modifier exclusions just as a keyboard-map change does.
            if matches!(
                event.request,
                xproto::Mapping::KEYBOARD | xproto::Mapping::MODIFIER
            ) {
                if let Err(error) = keyboard.reload(connection) {
                    tracing::debug!("X11 keyboard mapping refresh failed: {error}");
                }
            }
        }
    }
}

fn absolute(
    connection: &impl Connection,
    root: u32,
    state: &DesktopState,
    x: f64,
    y: f64,
) -> Result<(), String> {
    let Some(screen) = state.active_screen() else {
        return Ok(());
    };
    let px = super::normalized_to_pixel(x, screen.width);
    let py = super::normalized_to_pixel(y, screen.height);
    let root_x = (f64::from(screen.x) + px)
        .round()
        .clamp(0.0, f64::from(i16::MAX)) as i16;
    let root_y = (f64::from(screen.y) + py)
        .round()
        .clamp(0.0, f64::from(i16::MAX)) as i16;
    xtest::fake_input(
        connection,
        xproto::MOTION_NOTIFY_EVENT,
        0,
        x11rb::CURRENT_TIME,
        root,
        root_x,
        root_y,
        0,
    )
    .map_err(|error| error.to_string())?
    .check()
    .map_err(|error| error.to_string())
}

fn relative(connection: &impl Connection, dx: i32, dy: i32) -> Result<(), String> {
    let dx = dx.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    let dy = dy.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    xtest::fake_input(
        connection,
        xproto::MOTION_NOTIFY_EVENT,
        1,
        x11rb::CURRENT_TIME,
        x11rb::NONE,
        dx,
        dy,
        0,
    )
    .map_err(|error| error.to_string())?
    .check()
    .map_err(|error| error.to_string())
}

fn button(
    connection: &impl Connection,
    wire: i32,
    pressed: bool,
    held: &mut HashSet<u8>,
) -> Result<(), String> {
    let Some(button) = x_button(wire) else {
        return Ok(());
    };
    button_event(connection, button, pressed)?;
    if pressed {
        held.insert(button);
    } else {
        held.remove(&button);
    }
    Ok(())
}

fn button_event(connection: &impl Connection, button: u8, pressed: bool) -> Result<(), String> {
    let kind = if pressed {
        xproto::BUTTON_PRESS_EVENT
    } else {
        xproto::BUTTON_RELEASE_EVENT
    };
    xtest::fake_input(
        connection,
        kind,
        button,
        x11rb::CURRENT_TIME,
        x11rb::NONE,
        0,
        0,
        0,
    )
    .map_err(|error| error.to_string())?
    .check()
    .map_err(|error| error.to_string())
}

/// UU wire buttons are 1 = left, 2 = right, 4 = middle; X11 uses 1/3/2.
fn x_button(wire: i32) -> Option<u8> {
    match wire {
        1 => Some(1),
        2 => Some(3),
        4 => Some(2),
        _ => None,
    }
}

/// Wheel accumulation in notches. Positive `dy` is up and positive `dx` is
/// right, matching the Windows delta the official client forwards.
#[derive(Default)]
struct Wheel {
    x: f64,
    y: f64,
}

impl Wheel {
    /// Accumulate a delta and return the whole notches to emit.
    fn push(&mut self, dx: i32, dy: i32) -> (i32, i32) {
        self.x = (self.x + notches(dx)).clamp(-100.0, 100.0);
        self.y = (self.y + notches(dy)).clamp(-100.0, 100.0);
        let vertical = self.y.trunc();
        self.y -= vertical;
        let horizontal = self.x.trunc();
        self.x -= horizontal;
        (horizontal as i32, vertical as i32)
    }

    fn scroll(&mut self, connection: &impl Connection, dx: i32, dy: i32) -> Result<(), String> {
        let (horizontal, vertical) = self.push(dx, dy);
        for _ in 0..vertical.unsigned_abs() {
            let button = if vertical > 0 { 4 } else { 5 };
            wheel_click(connection, button)?;
        }
        for _ in 0..horizontal.unsigned_abs() {
            let button = if horizontal > 0 { 7 } else { 6 };
            wheel_click(connection, button)?;
        }
        Ok(())
    }
}

/// Normalize the three delta shapes the official client sends: notch counts,
/// Windows 120-unit deltas, and small smooth deltas.
fn notches(value: i32) -> f64 {
    let value = f64::from(value);
    let magnitude = value.abs();
    if magnitude == 0.0 {
        0.0
    } else if magnitude <= 5.0 {
        value
    } else if magnitude >= 100.0 {
        value / NOTCH
    } else {
        value / 10.0
    }
}

fn wheel_click(connection: &impl Connection, button: u8) -> Result<(), String> {
    for kind in [xproto::BUTTON_PRESS_EVENT, xproto::BUTTON_RELEASE_EVENT] {
        xtest::fake_input(
            connection,
            kind,
            button,
            x11rb::CURRENT_TIME,
            x11rb::NONE,
            0,
            0,
            0,
        )
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

struct Held {
    keycode: u8,
    shift: Option<u8>,
}

/// The server's live keyboard map plus a spare keycode for text.
struct Keyboard {
    min: u8,
    max: u8,
    per: u8,
    keysyms: Vec<u32>,
    shift: Option<u8>,
    spare: Option<u8>,
    spare_row: Vec<u32>,
    /// Number of held keys that need Shift down. Synthetic shifts and a
    /// client-held Shift share one server key state, so the key is released
    /// only when the last of them is released.
    shift_refs: usize,
}

impl Keyboard {
    fn load(connection: &impl Connection) -> Result<Self, String> {
        let setup = connection.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        let mut keysyms = Vec::new();
        let mut per = 0u8;
        let mut first = min;
        loop {
            let remaining = u16::from(max) - u16::from(first) + 1;
            let count = remaining.min(255) as u8;
            if count == 0 {
                break;
            }
            let reply = xproto::get_keyboard_mapping(connection, first, count)
                .map_err(|error| error.to_string())?
                .reply()
                .map_err(|error| error.to_string())?;
            if per == 0 {
                per = reply.keysyms_per_keycode;
            }
            keysyms.extend_from_slice(&reply.keysyms);
            let after = u16::from(first) + u16::from(count);
            if after > u16::from(max) {
                break;
            }
            first = after as u8;
        }
        if per == 0 {
            return Err("X11 returned an empty keyboard map".to_owned());
        }
        let modifiers = xproto::get_modifier_mapping(connection)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        let per_modifier = modifiers.keycodes_per_modifier() as usize;
        let shift = modifiers
            .keycodes
            .iter()
            .take(per_modifier)
            .copied()
            .find(|keycode| *keycode != 0);
        let mut keyboard = Self {
            min,
            max,
            per,
            keysyms,
            shift,
            spare: None,
            spare_row: Vec::new(),
            shift_refs: 0,
        };
        keyboard.find_spare(&modifiers.keycodes);
        Ok(keyboard)
    }

    fn reload(&mut self, connection: &impl Connection) -> Result<(), String> {
        let mut reloaded = Self::load(connection)?;
        // Keys may still be held while the mapping changes.
        reloaded.shift_refs = self.shift_refs;
        *self = reloaded;
        Ok(())
    }

    fn find_spare(&mut self, modifier_keycodes: &[Keycode]) {
        for keycode in self.min..=self.max {
            let base = (keycode - self.min) as usize * self.per as usize;
            let Some(row) = self.keysyms.get(base..base + self.per as usize) else {
                continue;
            };
            if row.iter().any(|keysym| *keysym != 0) {
                continue;
            }
            if modifier_keycodes.contains(&keycode) {
                continue;
            }
            self.spare = Some(keycode);
            self.spare_row = row.to_vec();
            return;
        }
    }

    fn lookup(&self, keysym: u32) -> Option<(u8, u8)> {
        for keycode in self.min..=self.max {
            let base = (keycode - self.min) as usize * self.per as usize;
            let Some(row) = self.keysyms.get(base..base + self.per as usize) else {
                continue;
            };
            // Only the first group (levels 0 and 1) is reachable without a
            // group switch; a match at a higher level would type the wrong
            // character, so those keysyms go through the spare keycode.
            for (level, candidate) in row.iter().take(2).enumerate() {
                if *candidate == keysym {
                    return Some((keycode, level as u8));
                }
            }
        }
        None
    }

    fn key(
        &mut self,
        connection: &impl Connection,
        root: u32,
        vk: i32,
        pressed: bool,
        held: &mut HashMap<i32, Held>,
    ) -> Result<(), String> {
        let Some(keysym) = windows_vk_to_keysym(vk) else {
            return Ok(());
        };
        if pressed {
            if held.contains_key(&vk) {
                return Ok(());
            }
            match self.lookup(keysym) {
                Some((keycode, level)) => {
                    let is_shift_key = self.shift == Some(keycode);
                    let shifted = !is_keypad(keysym) && level % 2 == 1;
                    let owned_shift = if shifted {
                        // Another held key (or the client's own Shift) may
                        // already keep Shift down; only the first owner
                        // presses it, and the last release lets go. A Shift
                        // held outside this session is left to its holder.
                        if self.shift_refs == 0 && shift_down(connection, root) {
                            None
                        } else {
                            if self.shift_refs == 0 {
                                if let Some(shift) = self.shift {
                                    key_event(connection, shift, true)?;
                                }
                            }
                            self.shift_refs += 1;
                            self.shift
                        }
                    } else if is_shift_key {
                        self.shift_refs += 1;
                        Some(keycode)
                    } else {
                        None
                    };
                    key_event(connection, keycode, true)?;
                    held.insert(
                        vk,
                        Held {
                            keycode,
                            shift: owned_shift,
                        },
                    );
                }
                // Layouts without this keysym still accept it through the
                // spare keycode, which acts like a one-shot text key.
                None => self.type_keysym(connection, root, keysym)?,
            }
        } else if let Some(released) = held.remove(&vk) {
            self.release_key(connection, released)?;
        }
        Ok(())
    }

    /// Release one held key, keeping a shared Shift down while another held
    /// key or the client's own Shift still needs it.
    fn release_key(&mut self, connection: &impl Connection, released: Held) -> Result<(), String> {
        let is_shift_key = self.shift == Some(released.keycode);
        if !is_shift_key {
            key_event(connection, released.keycode, false)?;
        }
        match released.shift {
            Some(shift) => {
                self.shift_refs = self.shift_refs.saturating_sub(1);
                if self.shift_refs == 0 {
                    key_event(connection, shift, false)?;
                }
            }
            None if is_shift_key => {
                // Shift released without recorded ownership (for example
                // after a mapping refresh); release it directly.
                key_event(connection, released.keycode, false)?;
            }
            None => {}
        }
        Ok(())
    }

    /// Type one keysym. A keycode already carrying the keysym is used
    /// directly (with Shift when needed); anything else is typed through a
    /// spare keycode remapped for the duration of the press. The mapping is
    /// server-global, so it is restored immediately.
    fn type_keysym(
        &mut self,
        connection: &impl Connection,
        root: u32,
        keysym: u32,
    ) -> Result<(), String> {
        if let Some((keycode, level)) = self.lookup(keysym) {
            let need_shift = !is_keypad(keysym)
                && level % 2 == 1
                && self.shift_refs == 0
                && !shift_down(connection, root);
            if need_shift {
                if let Some(shift) = self.shift {
                    key_event(connection, shift, true)?;
                }
            }
            let result = key_event(connection, keycode, true)
                .and_then(|()| key_event(connection, keycode, false));
            if need_shift {
                if let Some(shift) = self.shift {
                    let _ = key_event(connection, shift, false);
                }
            }
            return result;
        }
        let Some(spare) = self.spare else {
            return Ok(());
        };
        let row = vec![keysym; self.per as usize];
        let result = xproto::change_keyboard_mapping(connection, 1, spare, self.per, &row)
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())
            .and_then(|()| connection.sync().map_err(|error| error.to_string()))
            .and_then(|()| key_event(connection, spare, true))
            .and_then(|()| key_event(connection, spare, false))
            .and_then(|()| connection.sync().map_err(|error| error.to_string()));
        let restore =
            xproto::change_keyboard_mapping(connection, 1, spare, self.per, &self.spare_row)
                .map_err(|error| error.to_string())?
                .check()
                .map_err(|error| error.to_string())
                .and_then(|()| connection.sync().map_err(|error| error.to_string()));
        result.and(restore)
    }

    fn restore(&mut self, connection: &impl Connection) {
        if let (Some(spare), false) = (self.spare, self.spare_row.is_empty()) {
            let _ =
                xproto::change_keyboard_mapping(connection, 1, spare, self.per, &self.spare_row);
            let _ = connection.sync();
        }
    }
}

fn key_event(connection: &impl Connection, keycode: u8, pressed: bool) -> Result<(), String> {
    let kind = if pressed {
        xproto::KEY_PRESS_EVENT
    } else {
        xproto::KEY_RELEASE_EVENT
    };
    xtest::fake_input(
        connection,
        kind,
        keycode,
        x11rb::CURRENT_TIME,
        x11rb::NONE,
        0,
        0,
        0,
    )
    .map_err(|error| error.to_string())?
    .check()
    .map_err(|error| error.to_string())
}

fn shift_down(connection: &impl Connection, root: u32) -> bool {
    xproto::query_pointer(connection, root)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| u16::from(reply.mask) & u16::from(xproto::KeyButMask::SHIFT) != 0)
        .unwrap_or(false)
}

fn release_all(
    connection: &impl Connection,
    keyboard: &mut Keyboard,
    held: &mut HashMap<i32, Held>,
    buttons: &mut HashSet<u8>,
) {
    for (_, entry) in held.drain() {
        let _ = keyboard.release_key(connection, entry);
    }
    for button in buttons.drain() {
        let _ = button_event(connection, button, false);
    }
}

/// Keypad keysyms are selected by NumLock through the XKB KEYPAD type;
/// synthesizing Shift would turn KP_7 into KP_Home.
fn is_keypad(keysym: u32) -> bool {
    (0xff80..=0xffbd).contains(&keysym)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_buttons_map_to_x11_numbers() {
        assert_eq!(x_button(1), Some(1));
        assert_eq!(x_button(2), Some(3));
        assert_eq!(x_button(4), Some(2));
        assert_eq!(x_button(3), None);
    }

    #[test]
    fn wheel_deltas_normalize_to_notches() {
        assert_eq!(notches(120), 1.0);
        assert_eq!(notches(-240), -2.0);
        assert_eq!(notches(1), 1.0);
        assert_eq!(notches(10), 1.0);
        assert_eq!(notches(0), 0.0);
    }

    #[test]
    fn wheel_accumulates_fractional_deltas() {
        let mut wheel = Wheel::default();
        // 180 units is one and a half notches; the half is kept.
        assert_eq!(wheel.push(180, 180), (1, 1));
        assert_eq!(wheel.push(180, 180), (2, 2));
        // Windows 120-unit deltas map to one notch each.
        assert_eq!(wheel.push(120, -120), (1, -1));
        // Notch counts up to five pass through unchanged.
        assert_eq!(wheel.push(5, 5), (5, 5));
        // Small smooth pixel deltas use ten units per notch.
        assert_eq!(wheel.push(10, 10), (1, 1));
    }

    #[test]
    fn text_keysyms_follow_the_x11_ranges() {
        assert_eq!(keysym_for('a'), Some(0x61));
        assert_eq!(keysym_for('你'), Some(0x0100_4f60));
        assert_eq!(keysym_for('\n'), Some(0xff0d));
        assert_eq!(keysym_for('\u{7}'), None);
    }

    #[test]
    fn only_keypad_keysyms_skip_shift_synthesis() {
        assert!(is_keypad(0xff80)); // KP_Space
        assert!(is_keypad(0xffb7)); // KP_7
        assert!(is_keypad(0xffbd)); // KP_Equal
        assert!(!is_keypad(0x31)); // '1'
        assert!(!is_keypad(0xffbe)); // F1
        assert!(!is_keypad(0x100_4f60)); // Unicode CJK keysym
    }
}
