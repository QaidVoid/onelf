//! Telling the user about a dependency set or GL build download: a
//! progress line on a terminal, a progress dialog or else notifications
//! for a launch started from a desktop, and an error dialog when a set
//! that cannot be had stops the app.
//!
//! The runtime carries no D-Bus client. Dialogs go through the host's
//! `zenity-rs` or `zenity`, else `kdialog` (driven through `gdbus`), and notifications
//! through `notify-send`, else `gdbus`; a host with none of them gets the
//! terminal messages alone. Every helper that answers runs with a
//! deadline, so a stuck notification daemon cannot hold the launch.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a download runs before it is worth a dialog or notification.
const NOTIFY_AFTER: Duration = Duration::from_secs(1);
/// How often a kdialog progress bar is updated, each a `gdbus` call.
const KDIALOG_EVERY: Duration = Duration::from_secs(1);
/// How often a shown notification is updated with the bytes so far.
const NOTIFY_EVERY: Duration = Duration::from_secs(3);
/// How often the terminal line is redrawn.
const REDRAW_EVERY: Duration = Duration::from_millis(200);
/// The longest a notification helper may take.
const HELPER_DEADLINE: Duration = Duration::from_secs(2);
/// How long `kdialog --progressbar` may take to answer: it prints its D-Bus
/// handle only once its helper has registered, which a cold desktop
/// start can delay.
const KDIALOG_START_DEADLINE: Duration = Duration::from_secs(10);
/// The width of a zenity progress dialog, in pixels, so a long build name
/// and its progress fit on one line.
const ZENITY_WIDTH: &str = "--width=420";
/// The width of a zenity error dialog, wider for the reason it carries,
/// which often names a URL.
const ZENITY_ERROR_WIDTH: &str = "--width=640";
/// How long a dialog tool must stay up to count as started: one that
/// cannot run, such as a binary built for a newer glibc, exits at once.
const STARTUP_GRACE: Duration = Duration::from_millis(250);

static APP: OnceLock<String> = OnceLock::new();
/// The notification a download is showing, which a later one replaces.
static SHOWN: Mutex<Option<u32>> = Mutex::new(None);

/// Name the app the messages speak for. The first name set wins.
pub fn set_app(name: &str) {
    let _ = APP.set(name.to_string());
}

fn app() -> &'static str {
    APP.get().map(String::as_str).unwrap_or("onelf")
}

/// Where the user sees what the runtime says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    Terminal,
    Desktop,
    Log,
}

fn surface() -> Surface {
    let display = ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty()));
    surface_for(std::io::stderr().is_terminal(), display)
}

fn surface_for(terminal: bool, display: bool) -> Surface {
    if terminal {
        Surface::Terminal
    } else if display {
        Surface::Desktop
    } else {
        Surface::Log
    }
}

/// One download of the build named `what`, reported as it goes.
pub(crate) struct Download {
    what: String,
    surface: Surface,
    started: Instant,
    last: Option<Instant>,
    spoke: bool,
    done: u64,
    dialog: Option<ProgressDialog>,
}

impl Download {
    pub(crate) fn begin(what: &str) -> Self {
        Download {
            what: what.to_string(),
            surface: surface(),
            started: Instant::now(),
            last: None,
            spoke: false,
            done: 0,
            dialog: None,
        }
    }

    /// `done` bytes are in, of `total` when the server said.
    pub(crate) fn progress(&mut self, done: u64, total: Option<u64>) {
        self.done = done;
        let now = Instant::now();
        let since = |at: Option<Instant>| at.map(|t| now.duration_since(t));
        match self.surface {
            Surface::Terminal => {
                if since(self.last).is_some_and(|d| d < REDRAW_EVERY) {
                    return;
                }
                eprint!(
                    "\r\x1b[Konelf-rt: downloading {}: {}",
                    self.what,
                    amount(done, total)
                );
                let _ = std::io::stderr().flush();
            }
            Surface::Desktop => {
                if now.duration_since(self.started) < NOTIFY_AFTER {
                    return;
                }
                let summary = format!("{} is downloading {}", app(), self.what);
                if !self.spoke {
                    self.dialog = ProgressDialog::open(&summary, total.is_some());
                }
                let every = match &self.dialog {
                    Some(ProgressDialog::Zenity(_)) => REDRAW_EVERY,
                    Some(ProgressDialog::Kdialog { .. }) => KDIALOG_EVERY,
                    None => NOTIFY_EVERY,
                };
                if since(self.last).is_some_and(|d| d < every) {
                    return;
                }
                let text = format!("{summary}: {}", amount(done, total));
                let shown = self
                    .dialog
                    .as_mut()
                    .is_some_and(|dialog| dialog.update(&text, percent(done, total)));
                if !shown {
                    if let Some(dialog) = self.dialog.take() {
                        dialog.close();
                    }
                    show(&summary, &amount(done, total), Urgency::Normal, 0);
                }
            }
            Surface::Log => {
                if self.spoke || now.duration_since(self.started) < NOTIFY_AFTER {
                    return;
                }
                eprintln!("onelf-rt: downloading {}", self.what);
            }
        }
        self.last = Some(now);
        self.spoke = true;
    }

    /// The download is over; `ok` says whether it succeeded. A failure is
    /// for the caller to report, since only it knows whether the app can
    /// go on.
    pub(crate) fn end(mut self, ok: bool) {
        if !self.spoke {
            return;
        }
        if let Some(dialog) = self.dialog.take() {
            dialog.close();
        }
        match (self.surface, ok) {
            (Surface::Terminal, true) => eprintln!(
                "\r\x1b[Konelf-rt: downloaded {}: {}",
                self.what,
                amount(self.done, None)
            ),
            (Surface::Terminal, false) => eprintln!(),
            (Surface::Desktop, true) => show(
                &format!("{} downloaded {}", app(), self.what),
                &amount(self.done, None),
                Urgency::Normal,
                3000,
            ),
            (Surface::Log, true) => eprintln!("onelf-rt: downloaded {}", self.what),
            _ => {}
        }
    }
}

/// How far along the download is, held below 100 until it ends, since
/// zenity closes itself at 100.
fn percent(done: u64, total: Option<u64>) -> Option<u64> {
    total
        .filter(|&t| t > 0)
        .map(|t| (done.saturating_mul(100) / t).min(99))
}

/// A progress dialog on the desktop, for as long as a download runs.
enum ProgressDialog {
    /// `zenity --progress`, which reads its progress from standard input.
    Zenity(std::process::Child),
    /// `kdialog --progressbar`, which answers with the D-Bus name and
    /// path its bar is driven through.
    Kdialog {
        gdbus: PathBuf,
        dest: String,
        path: String,
    },
}

impl ProgressDialog {
    /// Open one, through zenity or else kdialog with gdbus, or `None`
    /// when the host has neither. Without a known total the bar pulses.
    fn open(text: &str, measured: bool) -> Option<Self> {
        for zenity in zenities() {
            let mut cmd = Command::new(zenity);
            cmd.args(["--progress", "--no-cancel", "--auto-close", ZENITY_WIDTH])
                .arg(format!("--title={}", app()))
                .arg(format!("--text={}", markup_escape(text)));
            if !measured {
                cmd.arg("--pulsate");
            }
            let Ok(mut child) = cmd
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            if stays_up(&mut child) {
                return Some(ProgressDialog::Zenity(child));
            }
        }
        let kdialog = which("kdialog")?;
        let gdbus = which("gdbus")?;
        let reply = run_within(
            &[
                kdialog.display().to_string(),
                "--title".into(),
                app().into(),
                "--progressbar".into(),
                text.into(),
                "100".into(),
            ],
            KDIALOG_START_DEADLINE,
        )?;
        let (dest, path) = reply.trim().split_once(' ')?;
        Some(ProgressDialog::Kdialog {
            gdbus,
            dest: dest.into(),
            path: path.into(),
        })
    }

    /// Show `text` and `percent`, or `false` once the dialog is gone.
    fn update(&mut self, text: &str, percent: Option<u64>) -> bool {
        match self {
            ProgressDialog::Zenity(child) => {
                if !matches!(child.try_wait(), Ok(None)) {
                    return false;
                }
                let mut lines = format!("# {}\n", markup_escape(text));
                if let Some(p) = percent {
                    lines.push_str(&format!("{p}\n"));
                }
                child
                    .stdin
                    .as_mut()
                    .is_some_and(|stdin| stdin.write_all(lines.as_bytes()).is_ok())
            }
            ProgressDialog::Kdialog { gdbus, dest, path } => {
                let shown = run(&kdialog_label_args(gdbus, dest, path, text)).is_some();
                if let Some(p) = percent {
                    run(&kdialog_value_args(gdbus, dest, path, p));
                }
                shown
            }
        }
    }

    fn close(self) {
        match self {
            ProgressDialog::Zenity(mut child) => {
                drop(child.stdin.take());
                if matches!(child.try_wait(), Ok(None)) {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            ProgressDialog::Kdialog { gdbus, dest, path } => {
                run(&[
                    gdbus.display().to_string(),
                    "call".into(),
                    "--session".into(),
                    format!("--dest={dest}"),
                    format!("--object-path={path}"),
                    "--method=org.kde.kdialog.ProgressDialog.close".into(),
                ]);
            }
        }
    }
}

fn amount(done: u64, total: Option<u64>) -> String {
    let mb = |b: u64| b as f64 / 1_048_576.0;
    match total {
        Some(total) if total >= done => format!("{:.1} of {:.1} MB", mb(done), mb(total)),
        _ => format!("{:.1} MB", mb(done)),
    }
}

/// A set this launch needs could not be had, so the app will not start:
/// on a desktop, say so in a notification and an error dialog.
pub(crate) fn cannot_start(what: &str, why: &str) {
    if surface() != Surface::Desktop {
        return;
    }
    let summary = format!("{} cannot start", app());
    let body = format!("The {what} it needs could not be obtained: {why}");
    show(&summary, &body, Urgency::Critical, 0);
    dialog(&summary, &body);
}

/// Something went wrong that the app runs on without: on a desktop, say
/// so in a notification.
pub(crate) fn warn(summary: &str, body: &str) {
    if surface() == Surface::Desktop {
        show(&format!("{}: {summary}", app()), body, Urgency::Normal, 0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Urgency {
    Normal,
    Critical,
}

/// Show a notification, in place of the one a download is showing. A
/// `timeout_ms` of 0 leaves it to the notification daemon.
fn show(summary: &str, body: &str, urgency: Urgency, timeout_ms: i32) {
    let mut shown = SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    let replaces = *shown;
    let id = which("notify-send")
        .and_then(|tool| {
            run(&notify_send_args(
                &tool, replaces, summary, body, urgency, timeout_ms,
            ))
        })
        .and_then(|out| out.trim().parse::<u32>().ok())
        .or_else(|| {
            which("gdbus")
                .and_then(|tool| {
                    run(&gdbus_args(
                        &tool, replaces, summary, body, urgency, timeout_ms,
                    ))
                })
                .and_then(|out| gdbus_id(&out))
        });
    if id.is_some() {
        *shown = id;
    }
}

fn notify_send_args(
    tool: &Path,
    replaces: Option<u32>,
    summary: &str,
    body: &str,
    urgency: Urgency,
    timeout_ms: i32,
) -> Vec<String> {
    let mut args = vec![
        tool.display().to_string(),
        "--print-id".into(),
        format!("--app-name={}", app()),
        format!(
            "--urgency={}",
            if urgency == Urgency::Critical {
                "critical"
            } else {
                "normal"
            }
        ),
    ];
    if timeout_ms > 0 {
        args.push(format!("--expire-time={timeout_ms}"));
    }
    if let Some(id) = replaces {
        args.push(format!("--replace-id={id}"));
    }
    args.push("--".into());
    args.push(summary.into());
    args.push(body.into());
    args
}

fn gdbus_args(
    tool: &Path,
    replaces: Option<u32>,
    summary: &str,
    body: &str,
    urgency: Urgency,
    timeout_ms: i32,
) -> Vec<String> {
    let level = if urgency == Urgency::Critical { 2 } else { 1 };
    vec![
        tool.display().to_string(),
        "call".into(),
        "--session".into(),
        "--dest=org.freedesktop.Notifications".into(),
        "--object-path=/org/freedesktop/Notifications".into(),
        "--method=org.freedesktop.Notifications.Notify".into(),
        "--".into(),
        gvariant_string(app()),
        format!("uint32 {}", replaces.unwrap_or(0)),
        gvariant_string(""),
        gvariant_string(summary),
        gvariant_string(body),
        "@as []".into(),
        format!("{{'urgency': <byte {level}>}}"),
        format!("int32 {}", if timeout_ms > 0 { timeout_ms } else { -1 }),
    ]
}

/// `text` as a GVariant string literal, which is how `gdbus call` reads
/// each argument.
fn gvariant_string(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The id in `gdbus call`'s reply, such as `(uint32 7,)`.
fn gdbus_id(reply: &str) -> Option<u32> {
    reply
        .trim()
        .strip_prefix("(uint32 ")?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Show a modal error dialog and wait for the user to close it, through
/// the first dialog tool that starts.
fn dialog(summary: &str, body: &str) {
    let text = format!("{summary}\n\n{body}");
    let zenity_args = |tool: PathBuf| {
        vec![
            tool.display().to_string(),
            "--error".into(),
            ZENITY_ERROR_WIDTH.into(),
            format!("--title={}", app()),
            format!("--text={}", markup_escape(&text)),
        ]
    };
    let kdialog_args = |tool: PathBuf| {
        vec![
            tool.display().to_string(),
            "--title".into(),
            app().into(),
            "--error".into(),
            text.clone(),
        ]
    };
    let candidates = zenities()
        .map(zenity_args)
        .chain(which("kdialog").map(kdialog_args));
    for args in candidates {
        let Ok(mut child) = Command::new(&args[0])
            .args(&args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if stays_up(&mut child) {
            let _ = child.wait();
            return;
        }
    }
}

/// Whether `child` is still running after [`STARTUP_GRACE`]; one that
/// has already exited is reaped.
fn stays_up(child: &mut std::process::Child) -> bool {
    let deadline = Instant::now() + STARTUP_GRACE;
    while Instant::now() < deadline {
        if !matches!(child.try_wait(), Ok(None)) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// zenity-rs, then zenity: the two take the same arguments.
fn zenities() -> impl Iterator<Item = PathBuf> {
    ["zenity-rs", "zenity"].into_iter().filter_map(which)
}

/// zenity reads its text as Pango markup.
/// It also reads backslash escapes in the text first.
fn markup_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The `gdbus call` that sets the label of the kdialog progress bar at
/// `dest` and `path`.
fn kdialog_label_args(gdbus: &Path, dest: &str, path: &str, text: &str) -> Vec<String> {
    vec![
        gdbus.display().to_string(),
        "call".into(),
        "--session".into(),
        format!("--dest={dest}"),
        format!("--object-path={path}"),
        "--method=org.kde.kdialog.ProgressDialog.setLabelText".into(),
        "--".into(),
        gvariant_string(text),
    ]
}

/// The `gdbus call` that moves the kdialog progress bar to `percent`.
/// The value is a property of the bar, not a method.
fn kdialog_value_args(gdbus: &Path, dest: &str, path: &str, percent: u64) -> Vec<String> {
    vec![
        gdbus.display().to_string(),
        "call".into(),
        "--session".into(),
        format!("--dest={dest}"),
        format!("--object-path={path}"),
        "--method=org.freedesktop.DBus.Properties.Set".into(),
        "--".into(),
        gvariant_string("org.kde.kdialog.ProgressDialog"),
        gvariant_string("value"),
        format!("<int32 {percent}>"),
    ]
}

/// Run a helper and return its standard output, or `None` when it fails
/// or outlives the deadline.
fn run(args: &[String]) -> Option<String> {
    run_within(args, HELPER_DEADLINE)
}

/// [`run`] with its own deadline.
fn run_within(args: &[String], deadline: Duration) -> Option<String> {
    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + deadline;
    // A caller that left SIGCHLD ignored has the kernel reap the helper, so
    // its exit shows as ECHILD with no status: its output says whether it
    // worked, since every helper read here prints something when it does.
    let reaped =
        |e: &std::io::Error| e.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error());
    let unknown = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break false,
            Err(e) if reaped(&e) => break true,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    if unknown && out.trim().is_empty() {
        return None;
    }
    Some(out)
}

/// The executable `name` on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|path| {
            std::fs::metadata(path)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_wins_over_a_display() {
        assert_eq!(surface_for(true, true), Surface::Terminal);
        assert_eq!(surface_for(false, true), Surface::Desktop);
        assert_eq!(surface_for(false, false), Surface::Log);
    }

    #[test]
    fn amounts_name_the_total_when_known() {
        assert_eq!(amount(13_000_000, Some(50_331_648)), "12.4 of 48.0 MB");
        assert_eq!(amount(13_000_000, None), "12.4 MB");
    }

    #[test]
    fn gdbus_strings_are_quoted_literals() {
        assert_eq!(gvariant_string(r"it's a\b"), r"'it\'s a\\b'");
    }

    #[test]
    fn the_gdbus_reply_gives_the_id() {
        assert_eq!(gdbus_id("(uint32 7,)\n"), Some(7));
        assert_eq!(gdbus_id("Error: no daemon"), None);
    }

    #[test]
    fn notify_send_replaces_the_shown_notification() {
        let args = notify_send_args(
            Path::new("/usr/bin/notify-send"),
            Some(9),
            "-s",
            "body",
            Urgency::Critical,
            3000,
        );
        assert!(args.contains(&"--replace-id=9".to_string()));
        assert!(args.contains(&"--urgency=critical".to_string()));
        assert!(args.contains(&"--expire-time=3000".to_string()));
        let dashdash = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(&args[dashdash + 1..], ["-s", "body"]);
    }

    #[test]
    fn progress_stays_below_full_until_the_end() {
        assert_eq!(percent(50, Some(200)), Some(25));
        assert_eq!(percent(200, Some(200)), Some(99));
        assert_eq!(percent(10, None), None);
        assert_eq!(percent(10, Some(0)), None);
    }

    #[test]
    fn zenity_text_is_escaped_markup() {
        assert_eq!(markup_escape(r"a < b & c\n"), r"a &lt; b &amp; c\\n");
    }

    #[test]
    fn gdbus_notify_carries_typed_arguments() {
        let args = gdbus_args(
            Path::new("/usr/bin/gdbus"),
            Some(7),
            "it's",
            "body",
            Urgency::Critical,
            0,
        );
        let tail = &args[args.iter().position(|a| a == "--").unwrap() + 1..];
        assert_eq!(tail[1], "uint32 7");
        assert_eq!(tail[3], r"'it\'s'");
        assert_eq!(tail[5], "@as []");
        assert_eq!(tail[6], "{'urgency': <byte 2>}");
        assert_eq!(tail[7], "int32 -1");
    }

    #[test]
    fn the_kdialog_bar_moves_through_its_value_property() {
        let args = kdialog_value_args(Path::new("/usr/bin/gdbus"), "org.kde.kdialog-1", "/P", 42);
        assert!(args.contains(&"--method=org.freedesktop.DBus.Properties.Set".to_string()));
        assert!(args.ends_with(&[
            "'org.kde.kdialog.ProgressDialog'".to_string(),
            "'value'".to_string(),
            "<int32 42>".to_string(),
        ]));
    }
}
