pub mod crashdump;
pub mod syslog;

#[cfg(feature = "journald")]
pub mod journald;

/// Strip ANSI / VT100 escape sequences from `s`.
///
/// Handles two representations of ESC:
///   1. Real ESC byte (`\x1b`) — written by programs to a TTY / pipe.
///   2. rsyslog octal encoding (`#033`) — rsyslog escapes non-printable bytes
///      as `#<octal>` when writing to `/var/log/syslog`.
///
/// For each ESC, strips CSI sequences (`ESC [ <params> <final-byte>`) and
/// plain two-byte sequences (`ESC <char>`).
///
/// Short-circuits: returns the input unchanged (no allocation) when it
/// contains neither form of ESC.
pub(super) fn strip_ansi(s: &str) -> String {
    // M4: avoid allocation when there is nothing to strip.
    if !s.contains('\x1b') && !s.contains("#033") {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // --- real ESC byte (0x1b) ---
        if b[i] == 0x1b {
            i += 1;
            skip_after_esc(b, &mut i);
            continue;
        }
        // --- rsyslog octal-escaped ESC: literal "#033" ---
        if b[i] == b'#' && b.get(i + 1..i + 4) == Some(b"033") {
            i += 4;
            skip_after_esc(b, &mut i);
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    // Safety: we only skip ASCII bytes from valid UTF-8, so output is valid UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// After consuming an ESC (real or `#033`), skip the rest of the sequence.
fn skip_after_esc(b: &[u8], i: &mut usize) {
    match b.get(*i) {
        Some(b'[') => {
            // CSI: ESC [ <params/intermediates> <final-byte (letter)>
            *i += 1;
            while *i < b.len() {
                let ch = b[*i];
                *i += 1;
                if ch.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        Some(_) => {
            // Simple two-byte sequence (e.g. ESC M): skip one more byte.
            *i += 1;
        }
        None => {}
    }
}
