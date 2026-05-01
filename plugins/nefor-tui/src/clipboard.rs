//! System-clipboard write via OSC 52.
//!
//! OSC 52 is a terminal-supported escape sequence that lets a program ask
//! the host terminal to copy text to the system clipboard. The sequence is:
//!
//! ```text
//! ESC ] 52 ; c ; <base64-payload> BEL
//! ```
//!
//! `c` selects the "clipboard" target (vs. `p` for primary). The payload is
//! standard base64 of the UTF-8 text. We send `BEL` (`\x07`) as the
//! string-terminator since every modern terminal that honors OSC 52 accepts
//! it; the alternative `ESC \` (ST) is also legal but more typing for no
//! gain.
//!
//! ## Caveats
//!
//! - **Terminal opt-in is required.** Most modern terminals (kitty, wezterm,
//!   alacritty, foot, iTerm2 with the option enabled, recent xterm) honor
//!   OSC 52 by default. macOS Terminal.app and tmux without
//!   `set -g set-clipboard on` do not — the escape sequence is silently
//!   dropped on those.
//! - **No feature detection in v1.** The sequence is fire-and-forget; we
//!   don't query the terminal to see if it'll honor it. A non-honoring
//!   terminal silently no-ops, which is the same observable behavior as
//!   "user pressed nothing".
//! - **Goes to /dev/tty, not stdout.** stdout is the NCP channel back to
//!   the engine; writing escape codes there would corrupt the JSONL stream.
//!   Same lane as the alt-screen / mouse-capture setup in `main.rs`.

use std::io::{self, Write};
use std::process::{Command, Stdio};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

/// Try the platform-native clipboard helper before OSC 52. Returns `Ok(true)`
/// if a helper successfully accepted the text, `Ok(false)` if no helper is
/// applicable on this platform (caller should fall back to OSC 52), or
/// `Err(_)` if a helper was found but its invocation failed (caller should
/// also fall back, but may want to log).
///
/// Why try this first: OSC 52 depends on the outer terminal honoring the
/// escape sequence. macOS Terminal.app drops it entirely; iTerm2 needs an
/// opt-in preference; tmux needs `set-clipboard` not `off`. A direct
/// `pbcopy` / `wl-copy` / `xclip` invocation bypasses all that.
pub fn write_native(text: &str) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        run_clipboard_cmd("pbcopy", &[], text).map(|_| true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        Ok(false)
    }
}

#[cfg(target_os = "macos")]
fn run_clipboard_cmd(program: &str, args: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "{program} exited with status {status}"
        )));
    }
    Ok(())
}

/// Build the OSC 52 escape sequence for `text` as a UTF-8 byte string.
/// Pure function — no IO. Caller writes the result to /dev/tty (or any
/// terminal-bound writer).
///
/// The shape is `\x1b]52;c;<base64>\x07`. An empty `text` produces an
/// empty-payload OSC 52 sequence, which most terminals interpret as "clear
/// the clipboard"; we let the caller decide whether that's the desired
/// semantics rather than special-casing here.
pub fn osc52_sequence(text: &str) -> Vec<u8> {
    let encoded = STANDARD.encode(text.as_bytes());
    // Pre-allocate: 5 bytes prefix ("\x1b]52;c;") + payload + 1 byte BEL.
    let mut out = Vec::with_capacity(7 + encoded.len() + 1);
    out.extend_from_slice(b"\x1b]52;c;");
    out.extend_from_slice(encoded.as_bytes());
    out.push(0x07);
    out
}

/// Write the OSC 52 sequence for `text` to `writer`. Best-effort: a
/// non-honoring terminal will silently drop the sequence, which is fine —
/// observable behavior is "user pressed nothing".
///
/// Returns the underlying `io::Error` only when the *write itself* fails
/// (writer closed, disk full, etc.). Terminal-side honoring is invisible to
/// us; we never see a "your clipboard is full" reply.
pub fn write_osc52<W: Write>(writer: &mut W, text: &str) -> io::Result<()> {
    let bytes = osc52_sequence(text);
    writer.write_all(&bytes)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_empty_payload_has_correct_shape() {
        let bytes = osc52_sequence("");
        // ESC ] 5 2 ; c ; BEL  =  7 + 0 + 1 = 8 bytes.
        assert_eq!(bytes, b"\x1b]52;c;\x07");
    }

    #[test]
    fn osc52_ascii_payload_encodes_to_known_base64() {
        // "hi" -> base64("hi") -> "aGk=".
        let bytes = osc52_sequence("hi");
        assert_eq!(bytes, b"\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn osc52_multibyte_payload_round_trips_via_decoding() {
        let text = "café 漢字 \u{1F600}";
        let bytes = osc52_sequence(text);
        // Trim ESC ] 52 ; c ; prefix and trailing BEL.
        assert_eq!(&bytes[..7], b"\x1b]52;c;");
        assert_eq!(*bytes.last().expect("non-empty"), 0x07);
        let payload = &bytes[7..bytes.len() - 1];
        let decoded = STANDARD.decode(payload).expect("payload is valid base64");
        assert_eq!(decoded, text.as_bytes());
    }

    #[test]
    fn osc52_no_inner_bel_or_esc() {
        // Common gotcha: payloads that already contain BEL or ESC would
        // confuse the terminal's parser, but base64 encodes binary cleanly
        // so neither byte appears mid-sequence regardless of input.
        let bytes = osc52_sequence("\x07\x1b\x07test");
        // The only BEL byte is the trailing one; the only ESC is the leading.
        assert_eq!(bytes.iter().filter(|b| **b == 0x07).count(), 1);
        assert_eq!(bytes.iter().filter(|b| **b == 0x1b).count(), 1);
    }

    #[test]
    fn write_osc52_writes_full_sequence_to_buffer() {
        let mut buf: Vec<u8> = Vec::new();
        write_osc52(&mut buf, "hello").expect("write to Vec never fails");
        assert!(buf.starts_with(b"\x1b]52;c;"));
        assert_eq!(*buf.last().expect("non-empty"), 0x07);
        // Decode payload and verify round-trip.
        let payload = &buf[7..buf.len() - 1];
        let decoded = STANDARD.decode(payload).expect("base64");
        assert_eq!(decoded, b"hello");
    }
}
