use std::ffi::CStr;
use std::os::raw::c_char;

pub(crate) fn ascii_lower(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

pub(crate) fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

pub(crate) fn contains_icase_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| {
        w.iter()
            .zip(needle.iter())
            .all(|(a, b)| ascii_lower(*a) == ascii_lower(*b))
    })
}

pub(crate) fn starts_with_icase_bytes(haystack: &[u8], prefix: &[u8]) -> bool {
    if prefix.is_empty() || haystack.len() < prefix.len() {
        return false;
    }
    haystack[..prefix.len()]
        .iter()
        .zip(prefix.iter())
        .all(|(a, b)| ascii_lower(*a) == ascii_lower(*b))
}

/// Copies byte `i` of `s` onto `out` as UTF-8, for a rewrite that walks SQL
/// byte by byte: an ASCII byte as itself, the first byte of a multi-byte
/// character as that whole character, and a continuation byte as nothing, since
/// its character went in with its first byte.
///
/// `out.push(bytes[i] as char)` is the bug this replaces. It reads each byte as
/// a character of its own, so "Caché" came out as "CachÃ©", and each pass that
/// did it encoded the text again.
///
/// Every byte of a multi-byte character is 0x80 or above, and a lexical rewrite
/// only ever branches on ASCII, so all of a character's bytes reach the same
/// copy and the character arrives whole.
pub(crate) fn push_utf8_byte(out: &mut String, s: &str, i: usize) {
    let b = s.as_bytes()[i];
    if b.is_ascii() {
        out.push(b as char);
    } else if let Some(c) = s.get(i..).and_then(|rest| rest.chars().next()) {
        out.push(c);
    }
}

pub(crate) unsafe fn cstr_bytes<'a>(ptr: *const c_char) -> &'a [u8] {
    if ptr.is_null() {
        return &[];
    }
    CStr::from_ptr(ptr).to_bytes()
}
