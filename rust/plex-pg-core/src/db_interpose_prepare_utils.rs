use std::ffi::CStr;
use std::os::raw::c_char;

pub fn contains_ascii_icase(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

pub fn starts_with_ascii_icase(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

/// How many `?` placeholders `sql` carries: the ones outside string
/// literals, quoted identifiers and comments.
///
/// The PLACEHOLDER_MISMATCH check counted every `?` byte, so a title with a
/// question mark in it, or a comment, read as a placeholder the translation
/// had lost, and every such statement was logged as a mismatch.
pub fn count_bare_question_marks(sql: &[u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < sql.len() {
        match sql[i] {
            b'\'' | b'"' | b'`' => {
                let q = sql[i];
                i += 1;
                while i < sql.len() {
                    if sql[i] == q {
                        // A doubled quote is an escaped quote inside.
                        if i + 1 < sql.len() && sql[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                while i < sql.len() && sql[i] != b']' {
                    i += 1;
                }
                i += 1;
            }
            b'-' if i + 1 < sql.len() && sql[i + 1] == b'-' => {
                while i < sql.len() && sql[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < sql.len() && sql[i + 1] == b'*' => {
                i += 2;
                while i + 1 < sql.len() && !(sql[i] == b'*' && sql[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'?' => {
                n += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    n
}

#[cfg(test)]
mod placeholder_tests {
    use super::count_bare_question_marks;

    #[test]
    fn question_marks_in_literals_identifiers_and_comments_are_not_placeholders() {
        assert_eq!(count_bare_question_marks(b"SELECT ?, ?"), 2);
        assert_eq!(
            count_bare_question_marks(b"SELECT * FROM t WHERE title = 'what?'"),
            0
        );
        assert_eq!(
            count_bare_question_marks(b"SELECT * FROM t WHERE title = 'it''s?' AND id = ?"),
            1
        );
        assert_eq!(
            count_bare_question_marks(b"SELECT \"why?\" FROM t WHERE id = ?"),
            1
        );
        assert_eq!(count_bare_question_marks(b"SELECT [a?] FROM t"), 0);
        assert_eq!(count_bare_question_marks(b"-- really?\nSELECT ?"), 1);
        assert_eq!(count_bare_question_marks(b"SELECT /* ? */ ? FROM t"), 1);
        assert_eq!(count_bare_question_marks(b"SELECT :U1, ?NNN"), 1);
        assert_eq!(count_bare_question_marks(b""), 0);
        assert_eq!(count_bare_question_marks(b"SELECT 'unterminated"), 0);
    }
}

pub fn contains_icase_ptr(ptr: *const c_char, needle: &str) -> bool {
    if ptr.is_null() {
        return false;
    }
    let hay = unsafe { CStr::from_ptr(ptr).to_bytes() };
    contains_ascii_icase(hay, needle.as_bytes())
}
