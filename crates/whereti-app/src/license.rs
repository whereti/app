//! The supporter license (PLAN.md §44): a key signed with Ed25519, checked
//! offline. Nothing is unlocked by it; it says thank you, by name.
//!
//! A key is `WHERETI-` + base64url(payload) + `.` + base64url(signature),
//! where the payload is a small JSON:
//! `{"v":1,"name":"Ion Popescu","plan":"personal","seats":1,"order":"123","issued":"2026-10-25"}`.
//! The private key lives only in the licenses Worker (Software Hive's, not
//! public); the public key below is all whereti needs. whereti being MIT,
//! anyone could patch the check out; that is fine for a thank-you.

use std::path::PathBuf;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};

/// The production public key keys are checked with (made once with
/// whereti.licenses/scripts/new-key.mjs; the private half signs the keys).
/// scripts/package.ps1 -Release refuses to build with the development key.
pub const PUBLIC_KEY_HEX: &str = "60a6dc92f067e73023b7e51860847348776873770b15af4ead02574f5c1dcabb";
/// The development key, for tests and dev builds only (the release script
/// compares against it).
#[cfg_attr(not(test), allow(dead_code))]
pub const DEV_PUBLIC_KEY_HEX: &str = "94227185d97edf2e009d21471dda317df8c766a407391f3cb840ebd53ded5d2b";

const PREFIX: &str = "WHERETI-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// One person, all their PCs, for life.
    Personal,
    /// A company, per PC, for life.
    Business,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct License {
    pub name: String,
    pub plan: Plan,
    pub seats: u32,
    pub issued: String,
    /// The key as entered, kept so it can be shown and saved again.
    pub key: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LicenseError {
    /// Not shaped like a whereti key (a typo, a partial copy).
    Malformed,
    /// Shaped right, but not signed by Software Hive.
    BadSignature,
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    if s.len() != 64 {
        return None;
    }
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Checks a key and reads what it says.
pub fn parse(key: &str) -> Result<License, LicenseError> {
    parse_with(key, PUBLIC_KEY_HEX)
}

fn parse_with(key: &str, public_hex: &str) -> Result<License, LicenseError> {
    // e-mail clients wrap long lines: ignore every space and line break
    let compact: String = key.chars().filter(|c| !c.is_whitespace()).collect();
    let body = compact.strip_prefix(PREFIX).unwrap_or(&compact);
    let (payload_b64, sig_b64) = body.split_once('.').ok_or(LicenseError::Malformed)?;
    let payload = URL_SAFE_NO_PAD.decode(payload_b64).map_err(|_| LicenseError::Malformed)?;
    let sig: [u8; 64] = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or(LicenseError::Malformed)?;
    let public = hex32(public_hex).and_then(|k| VerifyingKey::from_bytes(&k).ok()).ok_or(LicenseError::BadSignature)?;
    public.verify_strict(&payload, &Signature::from_bytes(&sig)).map_err(|_| LicenseError::BadSignature)?;
    let v: serde_json::Value = serde_json::from_slice(&payload).map_err(|_| LicenseError::Malformed)?;
    let name = v["name"].as_str().unwrap_or("").trim().to_string();
    let plan = match v["plan"].as_str() {
        Some("business") => Plan::Business,
        _ => Plan::Personal,
    };
    Ok(License {
        name: if name.is_empty() { "-".into() } else { name },
        plan,
        seats: v["seats"].as_u64().unwrap_or(1).clamp(1, 100_000) as u32,
        issued: v["issued"].as_str().unwrap_or("").to_string(),
        key: format!("{PREFIX}{body}"),
    })
}

fn file() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("whereti").join("license.txt")
}

/// The saved license, if it is (still) valid.
pub fn load() -> Option<License> {
    parse(&std::fs::read_to_string(file()).ok()?).ok()
}

pub fn save(l: &License) -> std::io::Result<()> {
    let path = file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::shell::write_whole(&path, l.key.as_bytes())
}

pub fn remove() {
    let _ = std::fs::remove_file(file());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// The development key's private half: tests only; production keys are
    /// signed by the licenses Worker with a different key.
    const DEV_SEED_HEX: &str = "dd45feac210abe7c6e6343a36be4b5678b3c05536fd0e25cbefe923448fdd949";

    fn make(payload: &str) -> String {
        let key = SigningKey::from_bytes(&hex32(DEV_SEED_HEX).unwrap());
        let sig = key.sign(payload.as_bytes());
        format!("{PREFIX}{}.{}", URL_SAFE_NO_PAD.encode(payload), URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    }

    #[test]
    fn a_signed_key_is_read() {
        let k = make(r#"{"v":1,"name":"Ion Popescu","plan":"business","seats":3,"order":"9","issued":"2026-10-25"}"#);
        let l = parse_with(&k, DEV_PUBLIC_KEY_HEX).unwrap();
        assert_eq!(l.name, "Ion Popescu");
        assert_eq!(l.plan, Plan::Business);
        assert_eq!(l.seats, 3);
        assert_eq!(l.issued, "2026-10-25");
    }

    #[test]
    fn line_breaks_from_an_email_do_not_matter() {
        let k = make(r#"{"v":1,"name":"Ana","plan":"personal"}"#);
        let wrapped: String = k.chars().enumerate().flat_map(|(i, c)| if i % 20 == 19 { vec![c, '\n', ' '] } else { vec![c] }).collect();
        assert_eq!(parse_with(&wrapped, DEV_PUBLIC_KEY_HEX).unwrap().name, "Ana");
        // the prefix may be missing
        assert!(parse_with(k.trim_start_matches(PREFIX), DEV_PUBLIC_KEY_HEX).is_ok());
    }

    #[test]
    fn a_changed_name_breaks_the_signature() {
        let k = make(r#"{"v":1,"name":"Ana","plan":"personal"}"#);
        let (_, sig) = k.split_once('.').unwrap();
        let forged = format!("{PREFIX}{}.{sig}", URL_SAFE_NO_PAD.encode(r#"{"v":1,"name":"Mallory","plan":"personal"}"#));
        assert_eq!(parse_with(&forged, DEV_PUBLIC_KEY_HEX), Err(LicenseError::BadSignature));
    }

    #[test]
    fn rubbish_is_malformed() {
        assert_eq!(parse_with("hello", DEV_PUBLIC_KEY_HEX), Err(LicenseError::Malformed));
        assert_eq!(parse_with("WHERETI-abc.def", DEV_PUBLIC_KEY_HEX), Err(LicenseError::Malformed));
    }

    /// Made by whereti.licenses' scripts/issue.mjs (Node, the Worker's format)
    /// with the development key: what the Worker signs, whereti accepts.
    #[test]
    fn a_key_from_the_licenses_worker_is_accepted() {
        let k = "WHERETI-eyJ2IjoxLCJuYW1lIjoiSW9uIFBvcGVzY3UiLCJwbGFuIjoiYnVzaW5lc3MiLCJzZWF0cyI6Nywib3JkZXIiOiJtYW51YWwtMSIsImlzc3VlZCI6IjIwMjYtMTAtMjUifQ.6lnCn_cvoZgaXnXqFVKvXfx1Ux1IlJw0tGGyEITRq7rnVMY8U603zigRxooTJrJieTxCxH_MCjQ0FbcZfnfIBQ";
        let l = parse_with(k, DEV_PUBLIC_KEY_HEX).unwrap();
        assert_eq!((l.name.as_str(), l.plan, l.seats), ("Ion Popescu", Plan::Business, 7));
    }

    #[test]
    fn the_dev_key_pair_matches() {
        let key = SigningKey::from_bytes(&hex32(DEV_SEED_HEX).unwrap());
        assert_eq!(key.verifying_key().to_bytes(), hex32(DEV_PUBLIC_KEY_HEX).unwrap());
    }
}
