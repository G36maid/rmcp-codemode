//! JS-identifier normalization, pi-exact. (pi:
//! `packages/codemode/src/identifier.ts:5-12`.)
//!
//! The MCP layer (server crate, §8.1) is stricter and pre-empts this fallback
//! for upstream tools; this module is the engine-level rule applied to native
//! tool names and `js_name` defaults.

/// The identifier the script uses for a tool: `tools.<id>` where `<id>` is
/// `name` with every character invalid in a JS identifier replaced by `_`.
/// First character must be `[A-Za-z_$]`, the rest `[A-Za-z0-9_$]`; an empty
/// result becomes `_`. Case is preserved.
pub fn to_codemode_identifier(name: &str) -> String {
    let mut identifier = String::with_capacity(name.len());
    for (index, ch) in name.chars().enumerate() {
        let valid = if index == 0 {
            ch.is_ascii_alphabetic() || ch == '_' || ch == '$'
        } else {
            ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
        };
        identifier.push(if valid { ch } else { '_' });
    }
    if identifier.is_empty() {
        "_".to_owned()
    } else {
        identifier
    }
}

#[cfg(test)]
mod tests {
    use super::to_codemode_identifier as ident;

    #[test]
    fn valid_names_pass_through() {
        assert_eq!(
            ident("mcp__github__list_issues"),
            "mcp__github__list_issues"
        );
        assert_eq!(ident("x"), "x");
        assert_eq!(ident("_x9$"), "_x9$");
    }

    #[test]
    fn invalid_characters_become_underscores() {
        // pi: the first-char rule applies only while the accumulator is
        // empty — a replaced first char makes later digits valid via the
        // rest rule, so "3d" eats the '3' (identifier.ts:5-12).
        assert_eq!(ident("3d"), "_d");
        assert_eq!(ident("my-server"), "my_server");
        assert_eq!(ident("user-profile/update"), "user_profile_update");
        assert_eq!(ident("a.b"), "a_b");
        assert_eq!(ident("héllo"), "h_llo");
    }

    #[test]
    fn empty_becomes_underscore() {
        assert_eq!(ident(""), "_");
    }
}
