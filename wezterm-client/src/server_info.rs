use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub pid: u32,
    pub version: String,
    pub started_at_unix: u64,
}

impl ServerInfo {
    pub fn parse(s: &str) -> Option<Self> {
        let mut pid = None;
        let mut version = None;
        let mut started_at_unix = None;

        let trimmed = s.trim();
        if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
            return None;
        }
        let inner = &trimmed[1..trimmed.len() - 1];

        let mut chars = inner.chars().peekable();

        while let Some(&c) = chars.peek() {
            if c.is_whitespace() || c == ',' {
                chars.next();
                continue;
            }

            if c != '"' {
                return None;
            }
            chars.next(); // consume opening quote of key

            let mut key = String::new();
            let mut closed = false;
            while let Some(c) = chars.next() {
                if c == '"' {
                    closed = true;
                    break;
                }
                key.push(c);
            }
            if !closed {
                return None;
            }

            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    chars.next();
                } else {
                    break;
                }
            }

            if let Some(':') = chars.peek() {
                chars.next();
            } else {
                return None;
            }

            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    chars.next();
                } else {
                    break;
                }
            }

            match chars.peek() {
                Some(&'"') => {
                    chars.next(); // consume opening quote of string value
                    let mut val_str = String::new();
                    let mut escaped = false;
                    let mut val_closed = false;
                    while let Some(c) = chars.next() {
                        if escaped {
                            match c {
                                'n' => val_str.push('\n'),
                                'r' => val_str.push('\r'),
                                't' => val_str.push('\t'),
                                '\\' => val_str.push('\\'),
                                '"' => val_str.push('"'),
                                '/' => val_str.push('/'),
                                other => {
                                    val_str.push('\\');
                                    val_str.push(other);
                                }
                            }
                            escaped = false;
                        } else if c == '\\' {
                            escaped = true;
                        } else if c == '"' {
                            val_closed = true;
                            break;
                        } else {
                            val_str.push(c);
                        }
                    }
                    if !val_closed {
                        return None;
                    }
                    if key == "version" {
                        version = Some(val_str);
                    }
                }
                Some(&c) if c.is_ascii_digit() || c == '-' => {
                    let mut num_str = String::new();
                    while let Some(&c) = chars.peek() {
                        if c.is_ascii_digit() || c == '-' {
                            num_str.push(c);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if key == "pid" {
                        pid = num_str.parse::<u32>().ok();
                    } else if key == "started_at_unix" {
                        started_at_unix = num_str.parse::<u64>().ok();
                    }
                }
                _ => {
                    let mut depth = 0;
                    while let Some(&c) = chars.peek() {
                        if c == '{' || c == '[' {
                            depth += 1;
                            chars.next();
                        } else if c == '}' || c == ']' {
                            if depth > 0 {
                                depth -= 1;
                            }
                            chars.next();
                        } else if c == ',' && depth == 0 {
                            break;
                        } else {
                            chars.next();
                        }
                    }
                }
            }
        }

        Some(Self {
            pid: pid?,
            version: version?,
            started_at_unix: started_at_unix?,
        })
    }

    pub fn is_stale_with(&self, now_unix: u64, is_pid_alive: impl Fn(u32) -> bool) -> bool {
        if self.pid == 0 || self.started_at_unix == 0 {
            return true;
        }
        if self.started_at_unix > now_unix.saturating_add(300) {
            return true;
        }
        if !is_pid_alive(self.pid) {
            return true;
        }
        false
    }

    pub fn is_stale(&self) -> bool {
        let now_unix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.is_stale_with(now_unix, default_is_pid_alive)
    }
}

// ponytail: kill(pid,0) cannot distinguish a reused pid; the check is
// advisory — the file is removed on clean shutdown and rewritten by any
// new server, so upgrade to a signed server identity if that ever matters.
pub fn default_is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid as libc::pid_t, 0) == 0 {
            true
        } else {
            let err = std::io::Error::last_os_error().raw_os_error();
            err == Some(libc::EPERM)
        }
    }
    #[cfg(not(unix))]
    {
        procinfo::LocalProcessInfo::with_root_pid(pid).is_some()
    }
}

pub fn read_server_info_from_file(path: &Path) -> Option<ServerInfo> {
    let content = std::fs::read_to_string(path).ok()?;
    let info = ServerInfo::parse(&content)?;
    if info.is_stale() {
        log::debug!("server-info at {} is stale: {:?}", path.display(), info);
        return None;
    }
    Some(info)
}

pub fn read_server_info(runtime_dir: &Path) -> Option<ServerInfo> {
    let path = runtime_dir.join("server-info.json");
    read_server_info_from_file(&path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheckResult {
    Match,
    Mismatch {
        server_version: String,
        client_version: String,
    },
    SkippedAbsentOrStale,
}

pub fn check_version_compat(
    server_info: Option<&ServerInfo>,
    client_version: &str,
) -> VersionCheckResult {
    match server_info {
        None => VersionCheckResult::SkippedAbsentOrStale,
        Some(info) if info.version == client_version => VersionCheckResult::Match,
        Some(info) => VersionCheckResult::Mismatch {
            server_version: info.version.clone(),
            client_version: client_version.to_string(),
        },
    }
}

pub fn should_notify_mismatch(result: &VersionCheckResult) -> bool {
    matches!(result, VersionCheckResult::Mismatch { .. })
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parse_valid_server_info() {
        let json = r#"{"pid": 42123, "version": "20260918-123456-abcdef", "started_at_unix": 1773837986}"#;
        let info = ServerInfo::parse(json).expect("should parse");
        assert_eq!(info.pid, 42123);
        assert_eq!(info.version, "20260918-123456-abcdef");
        assert_eq!(info.started_at_unix, 1773837986);
    }

    #[test]
    fn parse_server_info_with_whitespace_and_order() {
        let json = r#"
        {
            "version" : "20260918.1" ,
            "started_at_unix" : 1000 ,
            "pid" : 9999
        }
        "#;
        let info = ServerInfo::parse(json).expect("should parse");
        assert_eq!(info.pid, 9999);
        assert_eq!(info.version, "20260918.1");
        assert_eq!(info.started_at_unix, 1000);
    }

    #[test]
    fn parse_server_info_with_extra_fields() {
        let json = r#"{"pid": 123, "version": "v1", "started_at_unix": 456, "extra": "ignored"}"#;
        let info = ServerInfo::parse(json).expect("should parse");
        assert_eq!(info.pid, 123);
        assert_eq!(info.version, "v1");
        assert_eq!(info.started_at_unix, 456);
    }

    #[test]
    fn parse_server_info_missing_fields() {
        assert!(ServerInfo::parse(r#"{"pid": 123, "version": "v1"}"#).is_none());
        assert!(ServerInfo::parse(r#"{"version": "v1", "started_at_unix": 456}"#).is_none());
        assert!(ServerInfo::parse(r#"{"pid": 123, "started_at_unix": 456}"#).is_none());
        assert!(ServerInfo::parse("not json").is_none());
    }

    #[test]
    fn stale_detection() {
        let now = 1_000_000;
        let valid = ServerInfo {
            pid: 1234,
            version: "v1".to_string(),
            started_at_unix: now - 50,
        };
        assert!(!valid.is_stale_with(now, |_| true));

        // Dead pid is stale
        assert!(valid.is_stale_with(now, |_| false));

        // pid 0 is stale
        let zero_pid = ServerInfo {
            pid: 0,
            version: "v1".to_string(),
            started_at_unix: now - 50,
        };
        assert!(zero_pid.is_stale_with(now, |_| true));

        // started_at_unix 0 is stale
        let zero_time = ServerInfo {
            pid: 1234,
            version: "v1".to_string(),
            started_at_unix: 0,
        };
        assert!(zero_time.is_stale_with(now, |_| true));

        // Clock skew into future beyond 300s is stale
        let future_time = ServerInfo {
            pid: 1234,
            version: "v1".to_string(),
            started_at_unix: now + 301,
        };
        assert!(future_time.is_stale_with(now, |_| true));

        // Small clock skew into future (within 300s) is allowed
        let near_future = ServerInfo {
            pid: 1234,
            version: "v1".to_string(),
            started_at_unix: now + 10,
        };
        assert!(!near_future.is_stale_with(now, |_| true));
    }

    #[test]
    fn version_compare_and_decision() {
        let client_ver = "20260918-1";

        // Absent or stale -> SkippedAbsentOrStale
        let res = check_version_compat(None, client_ver);
        assert_eq!(res, VersionCheckResult::SkippedAbsentOrStale);
        assert!(!should_notify_mismatch(&res));

        // Matching version -> Match
        let matching = ServerInfo {
            pid: 100,
            version: client_ver.to_string(),
            started_at_unix: 500,
        };
        let res = check_version_compat(Some(&matching), client_ver);
        assert_eq!(res, VersionCheckResult::Match);
        assert!(!should_notify_mismatch(&res));

        // Mismatched version -> Mismatch
        let mismatched = ServerInfo {
            pid: 100,
            version: "20260917-old".to_string(),
            started_at_unix: 500,
        };
        let res = check_version_compat(Some(&mismatched), client_ver);
        assert_eq!(
            res,
            VersionCheckResult::Mismatch {
                server_version: "20260917-old".to_string(),
                client_version: client_ver.to_string(),
            }
        );
        assert!(should_notify_mismatch(&res));
    }

    #[test]
    fn read_server_info_file_handling() {
        let test_dir = std::env::temp_dir().join(format!("wezterm-test-server-info-{}", std::process::id()));
        std::fs::create_dir_all(&test_dir).unwrap();

        // 1. Absent file -> None
        assert!(read_server_info(&test_dir).is_none());

        // 2. Corrupt / invalid json -> None
        let file_path = test_dir.join("server-info.json");
        std::fs::write(&file_path, "not a json object").unwrap();
        assert!(read_server_info(&test_dir).is_none());

        // 3. Stale info with pid 0 -> None
        std::fs::write(&file_path, r#"{"pid": 0, "version": "v1", "started_at_unix": 100}"#).unwrap();
        assert!(read_server_info(&test_dir).is_none());

        // 4. Valid info with current process PID -> Some(info)
        let my_pid = std::process::id();
        let now_unix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::fs::write(
            &file_path,
            format!(
                r#"{{"pid": {}, "version": "test-v1", "started_at_unix": {}}}"#,
                my_pid, now_unix
            ),
        )
        .unwrap();
        let info = read_server_info(&test_dir).expect("should read valid server info");
        assert_eq!(info.pid, my_pid);
        assert_eq!(info.version, "test-v1");
        assert_eq!(info.started_at_unix, now_unix);

        std::fs::remove_dir_all(&test_dir).ok();
    }
}
