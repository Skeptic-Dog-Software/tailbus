//! Dialogs on this Mac, shown with `osascript`. Only a person at the screen
//! can answer one: nothing on the bus's socket or the network can.

#[cfg(test)]
use std::env;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

use crate::config::APP_NAME;
use anyhow::{Context, Result, bail};

/// Escape picks the first button, the "no" one. None is the default, so
/// pressing Return can't say yes.
const SCRIPT: &str = r#"on run argv
    set r to display dialog (item 2 of argv) with title (item 1 of argv) buttons (items 4 thru -1 of argv) cancel button 1 with icon caution giving up after (item 3 of argv as integer)
    if gave up of r then return ""
    return button returned of r
end run"#;

/// For tests: answers every dialog after a second, without showing it.
/// `no` picks the first button, `yes` the last, and anything else the
/// button with that label.
/// Answers every dialog without showing it, in this module's tests only: a
/// switch in a release build would let anything that sets the daemon's
/// environment answer its permission prompts.
#[cfg(test)]
const TEST_ANSWER: &str = "ACRBUS_TEST_ALERTS";

/// The most buttons a dialog can have.
pub const MAX_BUTTONS: usize = 3;

/// A dialog on screen.
pub struct Dialog {
    buttons: Vec<String>,
    shown: Mutex<Shown>,
}

enum Shown {
    Open(Child),
    #[cfg(test)]
    Test {
        choice: Option<usize>,
        at: Instant,
    },
    Closed,
}

impl Dialog {
    /// Shows a dialog with up to three buttons, the first of them the
    /// "no" one, which closes by itself after `timeout`.
    pub fn show(title: &str, text: &str, buttons: &[&str], timeout: Duration) -> Result<Dialog> {
        if buttons.is_empty() || buttons.len() > MAX_BUTTONS {
            bail!("a dialog has 1 to {MAX_BUTTONS} buttons");
        }
        let owned: Vec<String> = buttons.iter().map(|b| b.to_string()).collect();
        #[cfg(test)]
        if let Ok(answer) = env::var(TEST_ANSWER) {
            let choice = match answer.as_str() {
                "no" => Some(0),
                "yes" => Some(buttons.len() - 1),
                label => buttons.iter().position(|b| b.eq_ignore_ascii_case(label)),
            };
            return Ok(Dialog {
                buttons: owned,
                shown: Mutex::new(Shown::Test {
                    choice,
                    at: Instant::now() + Duration::from_secs(1),
                }),
            });
        }
        if !cfg!(target_os = "macos") {
            bail!("{APP_NAME} shows dialogs only on macOS");
        }
        let child = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(SCRIPT)
            .arg("--")
            .args([title, text])
            .arg(timeout.as_secs().max(1).to_string())
            .args(buttons)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running osascript")?;
        Ok(Dialog {
            buttons: owned,
            shown: Mutex::new(Shown::Open(child)),
        })
    }

    /// Waits for an answer: the index of the button clicked, or `None` when
    /// the dialog was closed, timed out, or couldn't be shown.
    pub fn answer(&self) -> Option<usize> {
        loop {
            {
                let mut shown = self.shown.lock().unwrap_or_else(|e| e.into_inner());
                match &mut *shown {
                    Shown::Closed => return None,
                    #[cfg(test)]
                    Shown::Test { choice, at } => {
                        if Instant::now() >= *at {
                            let choice = *choice;
                            *shown = Shown::Closed;
                            return choice;
                        }
                    }
                    Shown::Open(child) => match child.try_wait() {
                        Ok(None) => {}
                        Ok(Some(status)) => {
                            let mut out = String::new();
                            let mut err = String::new();
                            if let Some(mut s) = child.stdout.take() {
                                let _ = s.read_to_string(&mut out);
                            }
                            if let Some(mut s) = child.stderr.take() {
                                let _ = s.read_to_string(&mut err);
                            }
                            *shown = Shown::Closed;
                            return if status.success() {
                                self.buttons.iter().position(|b| b == out.trim())
                            } else if err.contains("-128") {
                                // "User canceled", from the first button or Escape.
                                Some(0)
                            } else {
                                None
                            };
                        }
                        Err(_) => {
                            *shown = Shown::Closed;
                            return None;
                        }
                    },
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
    }

    /// Takes the dialog off the screen, if it's still there.
    pub fn close(&self) {
        let mut shown = self.shown.lock().unwrap_or_else(|e| e.into_inner());
        if let Shown::Open(child) = &mut *shown {
            let _ = child.kill();
            let _ = child.wait();
        }
        *shown = Shown::Closed;
    }
}

/// Text from agents, made safe to show: no control or invisible characters
/// that could hide part of it, and at most `max` characters, cut in the
/// middle so the start and the end both show.
pub fn clean(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut newlines = 0;
    for c in text.chars() {
        let invisible = matches!(c,
            '\u{00AD}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}');
        match c {
            '\n' => {
                newlines += 1;
                if newlines <= 2 {
                    out.push('\n');
                }
            }
            '\t' => out.push(' '),
            c if c.is_control() || invisible => {}
            c => {
                newlines = 0;
                out.push(c);
            }
        }
    }
    let out = out.trim();
    let chars: Vec<char> = out.chars().collect();
    if chars.len() <= max {
        return out.to_string();
    }
    let head = max * 3 / 4;
    let tail = max - head;
    format!(
        "{}\n… {} characters left out …\n{}",
        chars[..head].iter().collect::<String>(),
        chars.len() - head - tail,
        chars[chars.len() - tail..].iter().collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_text_from_agents() {
        assert_eq!(clean("a\u{202E}b\u{200B}c\u{7}d", 100), "abcd");
        assert_eq!(clean("one\n\n\n\n\ntwo\tthree", 100), "one\n\ntwo three");
        let long = format!("{}{}", "a".repeat(100), "z".repeat(100));
        let cut = clean(&long, 40);
        assert!(cut.starts_with(&"a".repeat(30)));
        assert!(cut.ends_with(&"z".repeat(10)));
        assert!(cut.contains("160 characters left out"));
    }

    #[test]
    fn test_answers_come_after_a_second() {
        // SAFETY: tests in this module don't read the variable concurrently
        // with another writer.
        let buttons = ["Deny", "Allow", "Allow for session"];
        for (answer, want) in [
            ("no", Some(0)),
            ("yes", Some(2)),
            ("allow", Some(1)),
            ("x", None),
        ] {
            unsafe { env::set_var(TEST_ANSWER, answer) };
            let d = Dialog::show("t", "x", &buttons, Duration::from_secs(5)).unwrap();
            assert_eq!(d.answer(), want, "{answer}");
        }
        let d = Dialog::show("t", "x", &buttons, Duration::from_secs(5)).unwrap();
        d.close();
        assert_eq!(d.answer(), None);
        assert!(Dialog::show("t", "x", &[], Duration::from_secs(5)).is_err());
        unsafe { env::remove_var(TEST_ANSWER) };
    }
}
