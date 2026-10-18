//! Adding a certificate to the trust store of an environment.
//!
//! The PEM text goes to a POSIX script on stdin, so the same call reaches
//! devcontainers, VMs, remote machines and SSH hosts, whatever machine the
//! app itself runs on. The script finds the distribution's store, writes the
//! certificate under a fixed name and rebuilds the bundle, as root or through
//! passwordless `sudo`. Debian, Ubuntu and Alpine use `update-ca-certificates`;
//! Fedora, RHEL and Arch `update-ca-trust`.

use std::io;

use crate::{CommandError, Environment};

/// Reads a PEM certificate from stdin and adds it as `$1` to the system
/// store, and to `~/.pki/nssdb` when `certutil` is there. Prints the CA
/// bundle path.
const TRUST: &str = r#"set -u
name=$1
tmp=$(mktemp) || exit 1
trap 'rm -f "$tmp"' EXIT
cat > "$tmp"
grep -q 'BEGIN CERTIFICATE' "$tmp" || { echo "not a PEM certificate" >&2; exit 1; }
as_root() {
  if [ "$(id -u)" = 0 ]; then "$@"
  elif command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then sudo -n "$@"
  else echo "adding a certificate needs root or passwordless sudo in this environment" >&2; return 1
  fi
}
if command -v update-ca-certificates >/dev/null 2>&1; then
  as_root mkdir -p /usr/local/share/ca-certificates &&
  as_root cp "$tmp" "/usr/local/share/ca-certificates/$name.crt" &&
  as_root chmod 644 "/usr/local/share/ca-certificates/$name.crt" &&
  as_root update-ca-certificates >/dev/null || exit 1
elif command -v update-ca-trust >/dev/null 2>&1; then
  # Fedora and RHEL keep anchors under /etc/pki, Arch under /etc/ca-certificates.
  anchors=/etc/pki/ca-trust/source/anchors
  [ -d "$anchors" ] || anchors=/etc/ca-certificates/trust-source/anchors
  as_root mkdir -p "$anchors" &&
  as_root cp "$tmp" "$anchors/$name.pem" &&
  as_root chmod 644 "$anchors/$name.pem" &&
  as_root update-ca-trust extract || exit 1
else
  echo "no trust store tool found: install ca-certificates" >&2; exit 1
fi
if command -v certutil >/dev/null 2>&1; then
  db="$HOME/.pki/nssdb"
  mkdir -p "$db"
  [ -f "$db/cert9.db" ] || certutil -d "sql:$db" -N --empty-password >/dev/null 2>&1
  certutil -d "sql:$db" -D -n "$name" >/dev/null 2>&1
  certutil -d "sql:$db" -A -t "CP,," -n "$name" -i "$tmp" >/dev/null 2>&1
fi
for bundle in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/cert.pem; do
  if [ -f "$bundle" ]; then echo "$bundle"; break; fi
done
"#;

/// Whether `name` may name a certificate in a store: it becomes a file
/// name there.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

impl Environment {
    /// Adds the PEM certificate `pem` to the system trust store of this
    /// environment under `name` (letters, digits, `-`, `_` and `.`), replacing
    /// one of the same name. Returns the CA bundle path the environment
    /// uses, empty when it has none of the usual ones.
    ///
    /// This machine's own store is out of reach: a local environment, or a
    /// sandbox on it, is an error.
    pub fn trust_certificate(&self, name: &str, pem: &[u8]) -> Result<String, CommandError> {
        let refuse = |message: &str| {
            Err(CommandError::Spawn(io::Error::new(
                io::ErrorKind::InvalidInput,
                message.to_owned(),
            )))
        };
        if !valid_name(name) {
            return refuse("a certificate name is letters, digits, '-', '_' and '.'");
        }
        if matches!(self.base(), Environment::Local) && !matches!(self, Environment::Container(_)) {
            return refuse(
                "this machine's trust store is left to its own tools; \
                 trust works in devcontainers, VMs, remote machines and SSH hosts",
            );
        }
        let out = self
            .command("sh")
            .args(["-c", TRUST, "sh", name])
            .stdout_with_input(pem)?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_and_bad_names_are_refused() {
        let pem = b"-----BEGIN CERTIFICATE-----\n";
        assert!(Environment::Local.trust_certificate("dev", pem).is_err());
        let vm = Environment::Vm(crate::VmTarget { name: "x".into() });
        for name in ["", "../etc", ".hidden", "a b", &"x".repeat(65)] {
            let error = vm.trust_certificate(name, pem).unwrap_err();
            assert!(error.to_string().contains("certificate name"), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_script_is_valid_sh() {
        let status = std::process::Command::new("sh")
            .args(["-n", "-c", TRUST])
            .status()
            .unwrap();
        assert!(status.success());
    }
}
