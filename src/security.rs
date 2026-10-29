//! Request authorization, repository URL validation, and credential scoping.

use std::env;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::text::to_lower;

const DEFAULT_ALLOWED_HOSTS: &str = "github.com,gitlab.com,bitbucket.org";
const MAX_TOKEN_CHARS: usize = 4096;
const MIN_PRODUCTION_TOKEN_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryTarget {
    pub repository_url: String,
    pub repository_path: PathBuf,
}

/// Validates a per-request HTTPS Git credential without modifying it. Tokens
/// stay separate from repository URLs so they cannot leak via Git remotes,
/// command lines, or URL-oriented logging.
pub fn validate_auth_token(token: Option<&str>) -> Result<Option<String>, String> {
    let Some(token) = token else {
        return Ok(None);
    };
    if token.is_empty() {
        Err("auth_token must not be empty.".to_owned())
    } else if token.chars().count() > MAX_TOKEN_CHARS {
        Err("auth_token is too long.".to_owned())
    } else if token
        .chars()
        .any(|value| !value.is_ascii() || value.is_control())
    {
        Err("auth_token must contain printable ASCII characters only.".to_owned())
    } else {
        Ok(Some(token.to_owned()))
    }
}

/// Keeps public and authenticated clones in distinct namespaces. The token is
/// represented by a SHA-256 fingerprint: stable enough for request batching and
/// path isolation without placing the credential itself on disk.
pub fn repository_access_path(auth_token: Option<&str>, target: &RepositoryTarget) -> PathBuf {
    let scope = match auth_token {
        None => "public".to_owned(),
        Some(token) => {
            let digest = Sha256::digest(token.as_bytes());
            let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("authenticated-{hex}")
        }
    };
    PathBuf::from(scope).join(&target.repository_path)
}

fn trim_spaces(text: &str) -> &str {
    text.trim_matches(' ')
}

fn normalise_host(host: &str) -> String {
    trim_spaces(host).chars().map(to_lower).collect()
}

/// Reads `GIT_ALLOWED_HOSTS` (comma separated) at call time.
pub fn configured_allowed_hosts() -> Vec<String> {
    let configured = env::var_os("GIT_ALLOWED_HOSTS")
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| DEFAULT_ALLOWED_HOSTS.to_owned());
    configured
        .split(',')
        .map(normalise_host)
        .filter(|host| !host.is_empty())
        .collect()
}

fn valid_path_character(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.')
}

pub fn validate_repository_url(
    allowed_hosts: &[String],
    raw_url: &str,
) -> Result<RepositoryTarget, String> {
    let uri = parse_uri(raw_url).ok_or("Repository URL is invalid.")?;
    let authority = uri
        .authority
        .as_ref()
        .ok_or("Repository URL must include a host.")?;
    let host = normalise_host(&authority.reg_name);
    let segments: Vec<&str> = uri
        .path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let allowed: Vec<String> = allowed_hosts
        .iter()
        .map(|host| normalise_host(host))
        .collect();

    if uri.scheme != "https:" {
        return Err("Repository URL must use HTTPS.".to_owned());
    }
    if !authority.user_info.is_empty() {
        return Err("Repository URL must not contain embedded credentials.".to_owned());
    }
    if !authority.port.is_empty() {
        return Err("Repository URL must not specify a custom port.".to_owned());
    }
    if uri.query.is_some() || uri.fragment.is_some() {
        return Err("Repository URL must not contain a query string or fragment.".to_owned());
    }
    if !allowed.contains(&host) {
        return Err("Repository host is not allowed.".to_owned());
    }
    if segments.len() < 2 {
        return Err("Repository URL must include an owner and repository.".to_owned());
    }
    if !segments.iter().all(|segment| valid_segment(segment)) {
        return Err("Repository path contains unsupported characters.".to_owned());
    }

    let canonical = strip_git_suffix(&segments);
    // A suffix such as `..git` collapses to a dot segment, which would escape
    // the clone directory once joined into a path.
    if canonical
        .iter()
        .any(|segment| matches!(*segment, "." | ".."))
    {
        return Err("Repository path contains unsupported characters.".to_owned());
    }

    let repository_url = format!("https://{host}/{}", canonical.join("/"));
    let mut repository_path = PathBuf::from(&host);
    for segment in &canonical {
        repository_path.push(segment);
    }
    Ok(RepositoryTarget {
        repository_url,
        repository_path,
    })
}

fn valid_segment(segment: &str) -> bool {
    !matches!(segment, "." | "..") && segment.chars().all(valid_path_character)
}

fn strip_git_suffix<'a>(segments: &[&'a str]) -> Vec<&'a str> {
    let mut result = segments.to_vec();
    if let Some(last) = result.last_mut() {
        if last.len() > 4 && last.ends_with(".git") {
            *last = &last[..last.len() - 4];
        }
    }
    result
}

pub fn constant_time_equal(expected: &[u8], supplied: &[u8]) -> bool {
    expected.len() == supplied.len()
        && expected
            .iter()
            .zip(supplied)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

/// No configured key authorizes everything; otherwise a matching key is required.
pub fn api_key_authorized(expected: Option<&[u8]>, supplied: Option<&[u8]>) -> bool {
    match expected {
        None => true,
        Some(expected) => supplied.is_some_and(|supplied| constant_time_equal(expected, supplied)),
    }
}

/// Legacy `Data.ByteString.Char8.pack`: every character is truncated to 8 bits.
pub fn char8_bytes(text: &str) -> Vec<u8> {
    text.chars().map(|value| value as u32 as u8).collect()
}

/// The configured API token as the bytes requests are compared against.
pub fn configured_api_key() -> Option<Vec<u8>> {
    env::var_os("GIT_API_TOKEN").map(|value| char8_bytes(&value.to_string_lossy()))
}

pub fn headers_authorized(supplied: Option<&[u8]>) -> bool {
    api_key_authorized(configured_api_key().as_deref(), supplied)
}

pub fn validate_security_configuration() -> Result<(), String> {
    let environment = env::var_os("NODE_ENV").map(|value| value.to_string_lossy().into_owned());
    let api_token = env::var_os("GIT_API_TOKEN").map(|value| value.to_string_lossy().into_owned());
    if configured_allowed_hosts().is_empty() {
        return Err("GIT_ALLOWED_HOSTS must contain at least one host.".to_owned());
    }
    if environment.as_deref() == Some("production") {
        let long_enough = api_token
            .as_deref()
            .is_some_and(|token| char8_bytes(token).len() >= MIN_PRODUCTION_TOKEN_BYTES);
        if !long_enough {
            return Err("GIT_API_TOKEN must be at least 32 bytes in production.".to_owned());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// RFC 3986 URI parsing
//
// The repository URL is validated against its raw syntax, exactly like
// `Network.URI.parseURI`: nothing is normalised, so explicit ports, userinfo,
// dot segments and percent escapes remain visible to the checks above.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct Uri {
    /// Scheme including its trailing colon, e.g. `https:`.
    scheme: String,
    authority: Option<Authority>,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Authority {
    /// User information including its trailing `@`, or empty.
    user_info: String,
    /// Host text exactly as written (brackets kept for IP literals).
    reg_name: String,
    /// Port including its leading colon, or empty.
    port: String,
}

fn is_unreserved(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '-' | '.' | '_' | '~')
}

fn is_sub_delim(value: char) -> bool {
    matches!(
        value,
        '!' | '$' | '&' | '\'' | '(' | ')' | '*' | '+' | ',' | ';' | '='
    )
}

/// Checks that `text` consists of allowed characters and well-formed escapes.
fn valid_encoded(text: &str, extra: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let value = chars[index];
        if value == '%' {
            let escaped = chars.get(index + 1..index + 3);
            if !escaped.is_some_and(|pair| pair.iter().all(char::is_ascii_hexdigit)) {
                return false;
            }
            index += 3;
            continue;
        }
        if !(is_unreserved(value) || is_sub_delim(value) || extra.contains(value)) {
            return false;
        }
        index += 1;
    }
    true
}

fn valid_ip_literal(text: &str) -> bool {
    let Some(inner) = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    if let Some(future) = inner.strip_prefix(['v', 'V']) {
        return match future.split_once('.') {
            Some((version, tail)) => {
                !version.is_empty()
                    && version.chars().all(|value| value.is_ascii_hexdigit())
                    && !tail.is_empty()
                    && tail
                        .chars()
                        .all(|value| is_unreserved(value) || is_sub_delim(value) || value == ':')
            }
            None => false,
        };
    }
    inner.parse::<std::net::Ipv6Addr>().is_ok()
}

fn parse_authority(text: &str) -> Option<Authority> {
    let (user_info, host_port) = match text.split_once('@') {
        Some((user, rest)) => {
            if !valid_encoded(user, ":") {
                return None;
            }
            (format!("{user}@"), rest)
        }
        None => (String::new(), text),
    };
    let (reg_name, port) = if host_port.starts_with('[') {
        let close = host_port.find(']')?;
        let (literal, rest) = host_port.split_at(close + 1);
        if !valid_ip_literal(literal) {
            return None;
        }
        (literal, rest)
    } else {
        match host_port.find(':') {
            Some(colon) => host_port.split_at(colon),
            None => (host_port, ""),
        }
    };
    if !reg_name.starts_with('[') && !valid_encoded(reg_name, "") {
        return None;
    }
    if !port.is_empty()
        && !port
            .strip_prefix(':')
            .is_some_and(|digits| digits.chars().all(|value| value.is_ascii_digit()))
    {
        return None;
    }
    Some(Authority {
        user_info,
        reg_name: reg_name.to_owned(),
        port: port.to_owned(),
    })
}

fn parse_uri(raw: &str) -> Option<Uri> {
    let colon = raw.find(':')?;
    let scheme = &raw[..colon];
    let mut scheme_chars = scheme.chars();
    let first = scheme_chars.next()?;
    if !first.is_ascii_alphabetic()
        || !scheme_chars
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '+' | '-' | '.'))
    {
        return None;
    }
    let mut rest = &raw[colon + 1..];

    let fragment = match rest.split_once('#') {
        Some((before, fragment)) => {
            rest = before;
            Some(fragment)
        }
        None => None,
    };
    let query = match rest.split_once('?') {
        Some((before, query)) => {
            rest = before;
            Some(query)
        }
        None => None,
    };
    if !query.is_none_or(|value| valid_encoded(value, ":@/?")) {
        return None;
    }
    if !fragment.is_none_or(|value| valid_encoded(value, ":@/?")) {
        return None;
    }

    let (authority, path) = match rest.strip_prefix("//") {
        Some(after) => {
            let end = after.find('/').unwrap_or(after.len());
            (Some(parse_authority(&after[..end])?), &after[end..])
        }
        None => (None, rest),
    };
    if !valid_encoded(path, ":@/") {
        return None;
    }
    Some(Uri {
        scheme: format!("{scheme}:"),
        authority,
        path: path.to_owned(),
        query: query.map(str::to_owned),
        fragment: fragment.map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<String> {
        vec!["github.com".to_owned()]
    }

    fn rejected(url: &str) -> bool {
        validate_repository_url(&hosts(), url).is_err()
    }

    #[test]
    fn accepts_valid_github_url_and_strips_git_suffix() {
        let target =
            validate_repository_url(&hosts(), "https://github.com/Owner/Repo.git").unwrap();
        assert_eq!(target.repository_url, "https://github.com/Owner/Repo");
        let parts: Vec<_> = target
            .repository_path
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert_eq!(parts, ["github.com", "Owner", "Repo"]);
    }

    #[test]
    fn rejects_unsafe_repository_urls() {
        assert!(rejected("https://user:secret@github.com/Owner/Repo"));
        assert!(rejected("https://@github.com/Owner/Repo"));
        assert!(rejected("http://github.com/Owner/Repo"));
        assert!(rejected("HTTPS://github.com/Owner/Repo"));
        assert!(rejected("https://example.com/Owner/Repo"));
        assert!(rejected("https://github.com/Owner/Repo?token=secret"));
        assert!(rejected("https://github.com/Owner/Repo?"));
        assert!(rejected("https://github.com/Owner/Repo#frag"));
        assert!(rejected("https://github.com/Owner/Repo%22%3Bwhoami"));
        assert!(rejected("https://github.com:443/Owner/Repo"));
        assert!(rejected("https://github.com:/Owner/Repo"));
        assert!(rejected("https://github.com/Owner"));
        assert!(rejected("https://github.com/Owner/Re po"));
        assert!(rejected("https:github.com/Owner/Repo"));
        assert!(rejected("github.com/Owner/Repo"));
        assert!(rejected("https://github.com/../Repo"));
        assert!(rejected("https://github.com/Owner/é"));
    }

    #[test]
    fn error_precedence_follows_the_source() {
        let message = |url: &str| validate_repository_url(&hosts(), url).unwrap_err();
        assert_eq!(message("not a url"), "Repository URL is invalid.");
        assert_eq!(message("https:/a/b"), "Repository URL must include a host.");
        assert_eq!(
            message("http://u@evil.com:1/a/b?x"),
            "Repository URL must use HTTPS."
        );
        assert_eq!(
            message("https://u@evil.com:1/a/b?x"),
            "Repository URL must not contain embedded credentials."
        );
        assert_eq!(
            message("https://evil.com:1/a/b?x"),
            "Repository URL must not specify a custom port."
        );
        assert_eq!(
            message("https://evil.com/a/b?x"),
            "Repository URL must not contain a query string or fragment."
        );
        assert_eq!(
            message("https://evil.com/a"),
            "Repository host is not allowed."
        );
        assert_eq!(
            message("https://github.com/a"),
            "Repository URL must include an owner and repository."
        );
        assert_eq!(
            message("https://github.com/a/b%20c"),
            "Repository path contains unsupported characters."
        );
    }

    #[test]
    fn dot_segments_after_suffix_removal_are_rejected() {
        for url in [
            "https://github.com/owner/..git",
            "https://github.com/owner/...git",
            "https://github.com/owner/.",
        ] {
            assert!(rejected(url), "{url}");
        }
        let kept = validate_repository_url(&hosts(), "https://github.com/owner/.git").unwrap();
        assert_eq!(kept.repository_url, "https://github.com/owner/.git");
        let stripped =
            validate_repository_url(&hosts(), "https://github.com/owner/a..git").unwrap();
        assert_eq!(stripped.repository_url, "https://github.com/owner/a.");
    }

    #[test]
    fn host_matching_is_case_insensitive_and_canonical() {
        let target =
            validate_repository_url(&hosts(), "https://GitHub.COM//Owner///Repo/").unwrap();
        assert_eq!(target.repository_url, "https://github.com/Owner/Repo");
    }

    #[test]
    fn auth_tokens_are_validated() {
        assert_eq!(validate_auth_token(None), Ok(None));
        assert!(validate_auth_token(Some("")).is_err());
        assert!(validate_auth_token(Some("secret\nusername=attacker")).is_err());
        assert!(validate_auth_token(Some("tökén")).is_err());
        assert!(validate_auth_token(Some(&"a".repeat(4097))).is_err());
        assert_eq!(
            validate_auth_token(Some(&"a".repeat(4096))),
            Ok(Some("a".repeat(4096)))
        );
    }

    #[test]
    fn access_paths_are_scoped_by_credential() {
        let target =
            validate_repository_url(&hosts(), "https://github.com/Owner/Repo.git").unwrap();
        let secret = "private-repository-secret";
        let public = repository_access_path(None, &target);
        let private = repository_access_path(Some(secret), &target);
        let other = repository_access_path(Some("another-private-repository-secret"), &target);
        assert_eq!(public.components().next().unwrap().as_os_str(), "public");
        assert_ne!(private, public);
        assert_ne!(private, other);
        assert!(!private.to_string_lossy().contains(secret));
        assert_eq!(private, repository_access_path(Some(secret), &target));
        // SHA-256 of the UTF-8 token, lowercase hex.
        let digest = Sha256::digest(secret.as_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert!(private
            .to_string_lossy()
            .contains(&format!("authenticated-{hex}")));
    }

    #[test]
    fn api_keys_compare_in_constant_time_shape() {
        assert!(constant_time_equal(b"same-key", b"same-key"));
        assert!(!constant_time_equal(b"same-key", b"other-key"));
        assert!(!constant_time_equal(b"short", b"shorter"));
        assert!(api_key_authorized(None, None));
        assert!(!api_key_authorized(Some(b"expected"), None));
        assert!(!api_key_authorized(Some(b"expected"), Some(b"wrong")));
        assert!(api_key_authorized(Some(b"expected"), Some(b"expected")));
        assert!(api_key_authorized(Some(b""), Some(b"")));
        assert!(!api_key_authorized(Some(b""), None));
    }

    #[test]
    fn legacy_char8_truncates_characters() {
        assert_eq!(char8_bytes("aé"), vec![b'a', 0xe9]);
        assert_eq!(char8_bytes("Ā"), vec![0x00]);
    }

    #[test]
    fn uri_parser_keeps_raw_syntax() {
        let uri = parse_uri("https://user@Host.example:8080/a/./b?q#f").unwrap();
        assert_eq!(uri.scheme, "https:");
        let authority = uri.authority.unwrap();
        assert_eq!(authority.user_info, "user@");
        assert_eq!(authority.reg_name, "Host.example");
        assert_eq!(authority.port, ":8080");
        assert_eq!(uri.path, "/a/./b");
        assert_eq!(uri.query.as_deref(), Some("q"));
        assert_eq!(uri.fragment.as_deref(), Some("f"));
        assert!(parse_uri("https://[::1]:80/a").is_some());
        assert!(parse_uri("https://[nope]/a").is_none());
        assert!(parse_uri("https://a b/c").is_none());
        assert!(parse_uri("1http://a/b").is_none());
        assert!(parse_uri("https://a/%zz").is_none());
    }
}
