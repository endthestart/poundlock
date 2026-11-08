//! Credential files, which never leave the machine.

/// Credential files, never uploaded from any folder.
const SECRET_PATTERNS: &[&str] = &[
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    ".env",
    ".env.*",
    "!.env.example",
    "id_rsa",
    "id_rsa.*",
    "id_ed25519",
    "id_ed25519.*",
    "id_ecdsa",
    "id_ecdsa.*",
    "id_dsa",
    "id_dsa.*",
    "*.p8",
    "*.ppk",
    "*.jks",
    "*.keystore",
    "credentials.json",
    "*.secret.json",
    "secrets.json",
    "secrets.yml",
    "secrets.yaml",
    "secrets.toml",
    "auth.json",
    ".npmrc",
    ".pypirc",
    ".htpasswd",
    ".pgpass",
    ".dockercfg",
    "**/.aws/credentials",
    ".git-credentials",
    ".netrc",
    "_netrc",
    "**/.kube/config",
    "**/.docker/config.json",
    ".envrc",
    "*.tfstate",
    "*.tfstate.*",
    "*.tfvars",
    "*.tfvars.json",
    ".vault-token",
    "*service-account*.json",
    "*service_account*.json",
    "application_default_credentials.json",
    ".ssh/",
    "secrets/",
    ".kube/",
    ".docker/",
];

/// Whether `path` (relative, `/`-separated) is a credential file.
pub fn is_secret(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    let Some(name) = segments.last() else {
        return false;
    };
    let mut secret = false;
    for pattern in SECRET_PATTERNS {
        if let Some(exception) = pattern.strip_prefix('!') {
            if *name == exception {
                secret = false;
            }
            continue;
        }
        if pattern_matches(pattern, &segments) {
            secret = true;
        }
    }
    secret
}

fn pattern_matches(pattern: &str, segments: &[&str]) -> bool {
    let (directories, name) = segments.split_at(segments.len().saturating_sub(1));
    if let Some(directory) = pattern.strip_suffix('/') {
        return directories
            .iter()
            .any(|segment| wildcard(directory, segment));
    }
    if let Some(tail) = pattern.strip_prefix("**/") {
        let tail: Vec<&str> = tail.split('/').collect();
        return segments.len() >= tail.len()
            && segments[segments.len() - tail.len()..]
                .iter()
                .zip(&tail)
                .all(|(segment, part)| wildcard(part, segment));
    }
    name.first().is_some_and(|name| wildcard(pattern, name))
}

/// `*` matches any run of characters, `?` one character, within one name.
pub(super) fn wildcard(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}
