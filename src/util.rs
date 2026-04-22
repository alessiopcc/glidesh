/// Wrap a string in single quotes for safe shell interpolation.
/// Embedded single quotes are escaped as `'\''`.
pub fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
