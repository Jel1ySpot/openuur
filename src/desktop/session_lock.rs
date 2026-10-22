//! Session lock state and remote auto-unlock.
//!
//! The official host reports `SystemStateChange.SecureDesktopState` and
//! verifies the viewer-supplied password before unlocking. On Linux the lock
//! state comes from logind (`LockedHint`), the password is checked against
//! PAM's `login` service, and the existing session is unlocked through
//! `loginctl unlock-session`. The password is never stored or logged.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockResult {
    Success,
    WrongPassword,
    NoLoggedInUser,
    NoPermission,
    Failed,
}

/// Official ReportError.error_code for a failed auto unlock.
pub const UNLOCK_FAILED_CODE: i32 = -5;

fn loginctl(args: &[&str]) -> Option<String> {
    let output = Command::new("loginctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// The logind session that owns this user's graphical display.
pub fn display_session() -> Option<String> {
    let uid = unsafe { libc::getuid() }.to_string();
    if let Some(session) = loginctl(&["show-user", &uid, "-p", "Display", "--value"]) {
        let session = session.trim().to_owned();
        if !session.is_empty() && session != "-" {
            return Some(session);
        }
    }
    // Multiple seats (or an older logind): pick the active graphical session.
    if let Ok(output) = Command::new("loginctl")
        .args(["list-sessions", "--no-legend"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.split_whitespace();
            let Some(session) = fields.next() else {
                continue;
            };
            let Some(session_uid) = fields.next() else {
                continue;
            };
            if session_uid != uid {
                continue;
            }
            let kind =
                loginctl(&["show-session", session, "-p", "Type", "--value"]).unwrap_or_default();
            let active =
                loginctl(&["show-session", session, "-p", "Active", "--value"]).unwrap_or_default();
            if active == "yes" && matches!(kind.as_str(), "wayland" | "x11" | "mir") {
                return Some(session.to_owned());
            }
        }
    }
    None
}

pub fn session_locked(session: &str) -> Option<bool> {
    loginctl(&["show-session", session, "-p", "LockedHint", "--value"])
        .map(|value| value.eq_ignore_ascii_case("yes"))
}

pub fn locked_now() -> Option<bool> {
    display_session().as_deref().and_then(session_locked)
}

fn user_name() -> Option<String> {
    let pw = unsafe { libc::getpwuid(libc::getuid()) };
    if pw.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr((*pw).pw_name) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

// ---- PAM -----------------------------------------------------------------

const PAM_SUCCESS: c_int = 0;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type PamConvFn = unsafe extern "C" fn(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int;

#[repr(C)]
struct PamConv {
    conv: PamConvFn,
    appdata_ptr: *mut c_void,
}

type PamStart = unsafe extern "C" fn(
    service: *const c_char,
    user: *const c_char,
    conv: *const PamConv,
    pamh: *mut *mut c_void,
) -> c_int;
type PamAuthenticate = unsafe extern "C" fn(pamh: *mut c_void, flags: c_int) -> c_int;
type PamEnd = unsafe extern "C" fn(pamh: *mut c_void, status: c_int) -> c_int;

struct PamApi {
    start: PamStart,
    authenticate: PamAuthenticate,
    end: PamEnd,
    _library: *mut c_void,
}

unsafe impl Send for PamApi {}
unsafe impl Sync for PamApi {}

fn pam_api() -> Option<&'static PamApi> {
    static API: OnceLock<Option<PamApi>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let mut library = std::ptr::null_mut();
        for name in ["libpam.so.0", "libpam.so"] {
            let Ok(candidate) = CString::new(name) else {
                continue;
            };
            library = libc::dlopen(candidate.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if !library.is_null() {
                break;
            }
        }
        if library.is_null() {
            return None;
        }
        let symbol = |name: &str| {
            let Ok(name) = CString::new(name) else {
                return None;
            };
            let pointer = libc::dlsym(library, name.as_ptr());
            (!pointer.is_null()).then_some(pointer)
        };
        let start = symbol("pam_start")?;
        let authenticate = symbol("pam_authenticate")?;
        let end = symbol("pam_end")?;
        // Transmute the opaque symbol addresses to the concrete signatures.
        Some(PamApi {
            start: std::mem::transmute::<*mut c_void, PamStart>(start),
            authenticate: std::mem::transmute::<*mut c_void, PamAuthenticate>(authenticate),
            end: std::mem::transmute::<*mut c_void, PamEnd>(end),
            _library: library,
        })
    })
    .as_ref()
}

extern "C" fn pam_conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() {
        return 19; // PAM_CONV_ERR
    }
    let password = appdata as *const c_char;
    let count = num_msg as usize;
    let responses =
        unsafe { libc::calloc(count, std::mem::size_of::<PamResponse>()) } as *mut PamResponse;
    if responses.is_null() {
        return 5; // PAM_BUF_ERR
    }
    for index in 0..count {
        let message = unsafe { *msg.add(index) };
        let style = if message.is_null() {
            PAM_ERROR_MSG
        } else {
            unsafe { (*message).msg_style }
        };
        match style {
            PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                if password.is_null() {
                    unsafe { free_responses(responses, count) };
                    return 19;
                }
                let copy = unsafe { libc::strdup(password) };
                if copy.is_null() {
                    unsafe { free_responses(responses, count) };
                    return 5;
                }
                unsafe { (*responses.add(index)).resp = copy };
            }
            PAM_ERROR_MSG | PAM_TEXT_INFO => {}
            _ => {
                unsafe { free_responses(responses, count) };
                return 19;
            }
        }
    }
    unsafe { *resp = responses };
    PAM_SUCCESS
}

unsafe fn free_responses(responses: *mut PamResponse, count: usize) {
    for index in 0..count {
        let response = &mut *responses.add(index);
        if !response.resp.is_null() {
            libc::free(response.resp as *mut c_void);
            response.resp = std::ptr::null_mut();
        }
    }
    libc::free(responses as *mut c_void);
}

fn pam_available() -> bool {
    pam_api().is_some()
}

/// Verify the user's own password with the PAM `login` service.
/// Returns 0 for success, 1 for a wrong password, -1 when unavailable/error.
fn pam_check(user: &str, password: &str) -> i32 {
    let Some(api) = pam_api() else { return -1 };
    let (Ok(service), Ok(user), Ok(password)) = (
        CString::new("login"),
        CString::new(user),
        CString::new(password),
    ) else {
        return -1;
    };
    let conversation = PamConv {
        conv: pam_conversation,
        appdata_ptr: password.as_ptr() as *mut c_void,
    };
    let mut handle: *mut c_void = std::ptr::null_mut();
    let started =
        unsafe { (api.start)(service.as_ptr(), user.as_ptr(), &conversation, &mut handle) };
    let verdict = if started == PAM_SUCCESS && !handle.is_null() {
        let status = unsafe { (api.authenticate)(handle, 0) };
        unsafe { (api.end)(handle, status) };
        if status == PAM_SUCCESS {
            0
        } else {
            1
        }
    } else {
        -1
    };
    // Wipe the password copy; the CString owns the only heap copy.
    let mut bytes = password.into_bytes_with_nul();
    bytes.fill(0);
    verdict
}

/// Auto unlock is offered when a graphical logind session exists and PAM can
/// be loaded.
pub fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| pam_available() && display_session().is_some())
}

/// Lock state with the policy for unknown values applied: unknown counts as
/// unlocked so hosts without logind still serve input and capture.
/// `UU_LOCK_FAIL_CLOSED=1` makes unknown count as locked instead.
pub fn effective_locked(locked: Option<bool>) -> bool {
    locked.unwrap_or_else(|| std::env::var("UU_LOCK_FAIL_CLOSED").ok().as_deref() == Some("1"))
}

/// Current lock state (`None` = unknown), refreshed once per second.
pub fn watch_locked() -> watch::Receiver<Option<bool>> {
    static STATE: OnceLock<watch::Sender<Option<bool>>> = OnceLock::new();
    STATE
        .get_or_init(|| {
            let (sender, _) = watch::channel(None);
            let thread_sender = sender.clone();
            std::thread::Builder::new()
                .name("openuur-lock-state".into())
                .spawn(move || {
                    let mut session: Option<String> = None;
                    loop {
                        if session.is_none() {
                            session = display_session();
                        }
                        let locked = session.as_deref().and_then(session_locked);
                        if locked.is_none() {
                            session = None;
                        }
                        thread_sender.send_if_modified(|current| {
                            if *current != locked {
                                tracing::info!(
                                    "desktop lock state: {}",
                                    match locked {
                                        Some(true) => "locked",
                                        Some(false) => "unlocked",
                                        None => "unknown",
                                    }
                                );
                                *current = locked;
                                true
                            } else {
                                false
                            }
                        });
                        std::thread::sleep(Duration::from_secs(1));
                    }
                })
                .expect("spawn lock-state thread");
            sender
        })
        .subscribe()
}

/// Verify the password and unlock the existing display session. Blocking:
/// PAM may delay a failure, and `loginctl` waits for the compositor. Call from
/// `spawn_blocking`. Consumes and wipes the password.
pub fn unlock(mut password: String) -> UnlockResult {
    let result = unlock_with(&password);
    unsafe { password.as_bytes_mut().fill(0) };
    result
}

fn unlock_with(password: &str) -> UnlockResult {
    let Some(session) = display_session() else {
        return UnlockResult::NoLoggedInUser;
    };
    if session_locked(&session) == Some(false) {
        return UnlockResult::Success;
    }
    let Some(user) = user_name() else {
        return UnlockResult::NoLoggedInUser;
    };
    if password.is_empty() || password.contains('\0') {
        return UnlockResult::WrongPassword;
    }
    match pam_check(&user, password) {
        0 => {}
        1 => return UnlockResult::WrongPassword,
        _ => return UnlockResult::Failed,
    }
    let status = Command::new("loginctl")
        .args(["unlock-session", &session])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !status.is_ok_and(|status| status.success()) {
        return UnlockResult::NoPermission;
    }
    if let Err(error) = crate::desktop::desktop_power::wake_blocking() {
        tracing::warn!("display wake after auto unlock failed: {error}");
    }
    // Confirm the shield actually went away (GNOME takes a few seconds).
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if session_locked(&session) == Some(false) {
            return UnlockResult::Success;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    UnlockResult::Failed
}

/// The password from `SimpleAction.args` JSON (`{"key": "..."}`).
pub fn password_from_args(args: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(args).ok()?;
    value.get("key")?.as_str().map(str::to_owned)
}

/// `openuurd unlock-check`: print diagnostics; the password is read from
/// stdin and never echoed.
pub fn check_from_stdin() -> Result<(), String> {
    use std::io::BufRead;
    let session = display_session();
    println!(
        "display_session={} locked={:?} pam={} auto_unlock_supported={}",
        session.as_deref().unwrap_or("-"),
        session.as_deref().and_then(session_locked),
        pam_available(),
        supported()
    );
    let mut password = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut password)
        .map_err(|error| error.to_string())?;
    let mut password = password.trim_end_matches(['\r', '\n']).to_owned();
    let user = user_name().ok_or("no user name")?;
    let verdict = pam_check(&user, &password);
    unsafe { password.as_bytes_mut().fill(0) };
    println!(
        "password={}",
        match verdict {
            0 => "accepted",
            1 => "rejected",
            _ => "unverifiable (PAM unavailable/error)",
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_comes_from_the_official_key_field() {
        assert_eq!(
            password_from_args(br#"{"key":"p@ss word"}"#).as_deref(),
            Some("p@ss word")
        );
        assert_eq!(password_from_args(br#"{"password":"x"}"#), None);
        assert_eq!(password_from_args(b"not json"), None);
    }

    #[test]
    fn unlock_results_are_distinct() {
        assert_ne!(UnlockResult::Success, UnlockResult::WrongPassword);
        assert_eq!(UNLOCK_FAILED_CODE, -5);
    }
}
