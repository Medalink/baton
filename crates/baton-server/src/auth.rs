//! Who is connecting. Both providers fail closed: anything but a clear yes is a no.

use crate::db::Db;
use baton_core::{CommandSpec, MachineId, run_command};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

pub struct Identity {
    pub machine: MachineId,
    /// The device row, for the built-in provider. A revoked device ends its open sessions.
    pub device: Option<i64>,
}

pub enum Auth {
    Token(Arc<Db>),
    Command(CommandSpec),
}

#[derive(Serialize)]
struct AuthRequest<'a> {
    schema: &'static str,
    remote_addr: String,
    headers: &'a BTreeMap<String, String>,
    client_cert_sha256: Option<String>,
}

#[derive(Deserialize)]
struct AuthAnswer {
    ok: bool,
    machine: Option<String>,
}

fn bearer(headers: &BTreeMap<String, String>) -> Option<&str> {
    let value = headers.get("authorization")?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
        .filter(|t| !t.is_empty())
}

impl Auth {
    /// Blocking: the command provider starts a process.
    pub fn check(
        &self,
        headers: &BTreeMap<String, String>,
        remote: SocketAddr,
    ) -> Option<Identity> {
        match self {
            Self::Token(db) => {
                let device = db.device_for_token(bearer(headers)?).ok().flatten()?;
                Some(Identity {
                    machine: device.machine,
                    device: Some(device.id),
                })
            }
            Self::Command(spec) => {
                let request = AuthRequest {
                    schema: "baton.auth.v1",
                    remote_addr: remote.to_string(),
                    headers,
                    client_cert_sha256: None,
                };
                let answer: AuthAnswer = run_command(spec, &request).ok()?;
                let machine = answer.machine.filter(|_| answer.ok)?;
                Some(Identity {
                    machine: MachineId::parse(&machine).ok()?,
                    device: None,
                })
            }
        }
    }
}
