use std::{collections::BTreeMap, fmt, str::FromStr};

use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstObjectProp},
};
use serde::de::DeserializeOwned;

use super::{OwnedEntry, Server, env_overrides::EnvOverrides};

#[derive(Clone, Copy)]
pub(super) enum JsonClient {
    Standard,
    Copilot,
    VsCode,
}

impl JsonClient {
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Standard | Self::Copilot => "mcpServers",
            Self::VsCode => "servers",
        }
    }

    pub(super) fn options(self) -> ParseOptions {
        let comments = !matches!(self, Self::Standard);
        ParseOptions {
            allow_comments: comments,
            allow_trailing_commas: comments,
            allow_loose_object_property_names: false,
            allow_missing_commas: false,
            allow_single_quoted_strings: false,
            allow_hexadecimal_numbers: false,
            allow_unary_plus_numbers: false,
            allow_bare_decimal_point_numbers: false,
            allow_non_finite_numbers: false,
            allow_extended_string_escapes: false,
        }
    }
}

#[derive(Default, PartialEq, Eq)]
enum Transport {
    Local,
    #[default]
    Stdio,
}

impl FromStr for Transport {
    type Err = ConflictField;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "local" => Ok(Self::Local),
            "stdio" => Ok(Self::Stdio),
            _ => Err(ConflictField::Transport),
        }
    }
}

impl AsRef<str> for Transport {
    fn as_ref(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::Stdio => "stdio",
        }
    }
}

#[derive(PartialEq, Eq)]
struct ConfiguredServer {
    command: String,
    args: Vec<String>,
    transport: Transport,
    env: EnvOverrides,
    tools: Option<Vec<String>>,
}

pub(super) struct ManagedServer {
    client: JsonClient,
    spec: ConfiguredServer,
}

impl From<(JsonClient, Server, &str)> for ManagedServer {
    fn from((client, server, command): (JsonClient, Server, &str)) -> Self {
        let (transport, tools) = match client {
            JsonClient::Standard | JsonClient::VsCode => (Transport::default(), None),
            JsonClient::Copilot => (Transport::Local, Some(vec!["*".into()])),
        };
        Self {
            client,
            spec: ConfiguredServer {
                command: command.into(),
                args: server.arguments().iter().map(|arg| (*arg).into()).collect(),
                transport,
                env: EnvOverrides::default(),
                tools,
            },
        }
    }
}

impl From<ManagedServer> for CstInputValue {
    fn from(ManagedServer { client, spec }: ManagedServer) -> Self {
        let mut fields = vec![
            ("command".into(), spec.command.into()),
            ("args".into(), spec.args.into()),
        ];
        if !matches!(client, JsonClient::Standard) {
            fields.push(("type".into(), spec.transport.as_ref().into()));
        }
        if let Some(tools) = spec.tools {
            fields.push(("tools".into(), tools.into()));
        }
        fields.into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::agents) enum ConflictField {
    Command,
    Args,
    Env,
    Transport,
    Unknown(&'static str),
}

impl fmt::Display for ConflictField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Env => return f.write_str(
                "has a custom or invalid `env`. Remove `env` if the overrides are no longer needed, or rename the entry to keep it, then rerun bk agents connect",
            ),
            Self::Command => "command",
            Self::Args => "args",
            Self::Transport => "type",
            Self::Unknown(name) => name,
        };
        write!(
            f,
            "has a conflicting `{name}`. Correct the field or rename the entry, then rerun bk agents connect"
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::agents) enum ServerConflict {
    Field(ConflictField),
    Unrecognized(String),
}

impl From<ConflictField> for ServerConflict {
    fn from(field: ConflictField) -> Self {
        Self::Field(field)
    }
}

impl From<String> for ServerConflict {
    fn from(name: String) -> Self {
        match name.as_str() {
            "command" => ConflictField::Command.into(),
            "args" => ConflictField::Args.into(),
            "env" => ConflictField::Env.into(),
            "type" => ConflictField::Transport.into(),
            "tools" => ConflictField::Unknown("tools").into(),
            _ => Self::Unrecognized(name),
        }
    }
}

impl fmt::Display for ServerConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Field(field) => field.fmt(f),
            Self::Unrecognized(name) => write!(
                f,
                "has an unsupported field {name:?}. Remove the field or rename the entry, then rerun bk agents connect"
            ),
        }
    }
}

pub(super) struct ParsedServer {
    spec: ConfiguredServer,
    needs_rewrite: bool,
}

impl ParsedServer {
    pub(super) fn check(self, expected: &ManagedServer) -> Result<OwnedEntry, ServerConflict> {
        let expected = &expected.spec;
        if self.spec == *expected {
            return Ok(if self.needs_rewrite {
                OwnedEntry::NeedsRewrite
            } else {
                OwnedEntry::Canonical
            });
        }
        let field = if self.spec.env != expected.env {
            ConflictField::Env
        } else if self.spec.command != expected.command {
            ConflictField::Command
        } else if self.spec.args != expected.args {
            ConflictField::Args
        } else if self.spec.transport != expected.transport {
            ConflictField::Transport
        } else {
            ConflictField::Unknown("tools")
        };
        Err(field.into())
    }
}

impl TryFrom<(JsonClient, &CstObjectProp)> for ParsedServer {
    type Error = ServerConflict;

    fn try_from((client, entry): (JsonClient, &CstObjectProp)) -> Result<Self, Self::Error> {
        let mut fields = BTreeMap::new();
        let object = entry
            .object_value()
            .ok_or(ConflictField::Unknown("entry"))?;
        for property in object.properties() {
            let name = property
                .decoded_name()
                .ok_or(ConflictField::Unknown("entry"))?;
            if fields.insert(name.clone(), property).is_some() {
                return Err(name.into());
            }
        }
        let options = client.options();
        let transport = fields.remove("type");
        let env = fields.remove("env");
        let standard = matches!(client, JsonClient::Standard);
        let needs_rewrite = standard && (transport.is_some() || env.is_some());
        let env = match env {
            Some(_) if !standard => return Err(ConflictField::Env.into()),
            Some(env) => read(&env, &options, ConflictField::Env)?,
            None => EnvOverrides::default(),
        };
        let transport = match transport {
            Some(transport) => {
                read::<String>(&transport, &options, ConflictField::Transport)?.parse()?
            }
            None if standard => Transport::default(),
            None => return Err(ConflictField::Transport.into()),
        };
        let command = required(&mut fields, "command", &options, ConflictField::Command)?;
        let args = required(&mut fields, "args", &options, ConflictField::Args)?;
        let tools = fields
            .remove("tools")
            .map(|tools| read(&tools, &options, ConflictField::Unknown("tools")))
            .transpose()?;
        if let Some((name, _)) = fields.into_iter().next() {
            return Err(name.into());
        }
        Ok(Self {
            spec: ConfiguredServer {
                command,
                args,
                transport,
                env,
                tools,
            },
            needs_rewrite,
        })
    }
}

fn required<T: DeserializeOwned>(
    fields: &mut BTreeMap<String, CstObjectProp>,
    name: &'static str,
    options: &ParseOptions,
    field: ConflictField,
) -> Result<T, ConflictField> {
    read(&fields.remove(name).ok_or(field)?, options, field)
}

fn read<T: DeserializeOwned>(
    property: &CstObjectProp,
    options: &ParseOptions,
    field: ConflictField,
) -> Result<T, ConflictField> {
    let node = property.value().ok_or(field)?;
    jsonc_parser::parse_to_serde_value(&node.to_string(), options).map_err(|_| field)
}
