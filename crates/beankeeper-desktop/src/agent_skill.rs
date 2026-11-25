//! Publish the same skill that agent registration installs locally (SEP-2640).

use std::sync::LazyLock;

use rmcp::{
    ErrorData,
    model::{
        CacheScope, GetSkillRequestParams, GetSkillResult, ListResourceTemplatesResult,
        ListResourcesResult, ListSkillsResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResult, Resource, ResourceContents, SKILLS_EXTENSION_ID, Skill,
        SkillFrontmatter, SkillResource, SkillResources,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const SKILL: &str = include_str!("../../../SKILL.md");
const URI: &str = "skill://beankeeper-desktop/SKILL.md";
const MIME_TYPE: &str = "text/markdown";
const _: () = assert!(SKILL.len() <= 16 * 1024 * 1024);
static EMBEDDED: LazyLock<Result<EmbeddedSkill, ErrorData>> =
    LazyLock::new(|| EmbeddedSkill::try_from(SKILL));

pub(crate) fn voice_summary() -> String {
    paragraph_after("## How to talk to the owner")
}

pub(crate) fn claude_setup_summary() -> String {
    paragraph_after("- **Claude Desktop chat.**")
}

pub(crate) fn demo_summary() -> String {
    paragraph_after("### Tailor a demo in chat")
}

fn paragraph_after(marker: &str) -> String {
    SKILL
        .lines()
        .skip_while(|line| *line != marker)
        .skip(1)
        .map(str::trim)
        .skip_while(|line| line.is_empty())
        .take_while(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The private constructor keeps metadata, content and the advertised digest together.
struct EmbeddedSkill {
    entry: Skill,
    markdown: &'static str,
}

impl TryFrom<&'static str> for EmbeddedSkill {
    type Error = ErrorData;

    fn try_from(markdown: &'static str) -> Result<Self, Self::Error> {
        let invalid = || ErrorData::internal_error("Invalid embedded Beankeeper skill.", None);
        let (opening, rest) = markdown.split_once('\n').ok_or_else(invalid)?;
        let yaml_len = rest
            .split_inclusive('\n')
            .scan(0, |offset, line| {
                let start = *offset;
                *offset += line.len();
                Some((start, line))
            })
            .find_map(|(start, line)| {
                (line.trim_end_matches(['\r', '\n']) == "---").then_some(start)
            });
        let yaml_len = yaml_len
            .filter(|_| opening.trim_end_matches('\r') == "---")
            .ok_or_else(invalid)?;
        let frontmatter: SkillFrontmatter =
            yaml_serde::from_str(&rest[..yaml_len]).map_err(|_| invalid())?;
        if !(1..=1024).contains(&frontmatter.description.chars().count()) {
            return Err(invalid());
        }
        let files =
            SkillResources::Files(vec![SkillResource::from_bytes(URI, markdown.as_bytes())]);
        let entry = Skill::new(URI, frontmatter, files);
        entry.validate().map_err(|_| invalid())?;
        Ok(Self { entry, markdown })
    }
}

impl EmbeddedSkill {
    fn require_uri(&self, uri: &str) -> Result<(), ErrorData> {
        if uri == self.entry.uri {
            Ok(())
        } else {
            Err(ErrorData::invalid_params(
                "Only the Beankeeper Desktop skill is served here.",
                None,
            ))
        }
    }

    fn resource(&self) -> Resource {
        Resource::new(&self.entry.uri, self.entry.name())
            .with_description(&self.entry.frontmatter.description)
            .with_mime_type(MIME_TYPE)
            .with_size(self.markdown.len() as u64)
    }
}

pub(crate) fn handled(method: &str) -> bool {
    matches!(
        method,
        "skills/list"
            | "skills/get"
            | "resources/list"
            | "resources/read"
            | "resources/templates/list"
    )
}

pub(crate) fn advertise(capabilities: &mut Value) {
    capabilities["extensions"][SKILLS_EXTENSION_ID] = json!({});
    if capabilities.get("resources").is_none() {
        capabilities["resources"] = json!({});
    }
}

pub(crate) fn result(method: &str, params: &Value) -> Result<Value, ErrorData> {
    let skill = EMBEDDED.as_ref().map_err(Clone::clone)?;
    match method {
        "skills/get" => {
            let request = parse::<GetSkillRequestParams>(params)?;
            skill.require_uri(&request.uri)?;
            encode(
                GetSkillResult::new(skill.entry.clone())
                    .with_ttl_ms(0)
                    .with_cache_scope(CacheScope::Public),
            )
        }
        "resources/read" => {
            let request = parse::<ReadResourceRequestParams>(params)?;
            skill.require_uri(&request.uri)?;
            let contents =
                ResourceContents::text(skill.markdown, &skill.entry.uri).with_mime_type(MIME_TYPE);
            encode(
                ReadResourceResult::new(vec![contents])
                    .with_ttl_ms(0)
                    .with_cache_scope(CacheScope::Public),
            )
        }
        _ => {
            let request = parse::<Option<PaginatedRequestParams>>(params)?;
            if request.is_some_and(|page| page.cursor.is_some()) {
                return Err(ErrorData::invalid_params(
                    "The Beankeeper skill catalog has one page.",
                    None,
                ));
            }
            match method {
                "skills/list" => encode(
                    ListSkillsResult::with_all_items(vec![skill.entry.clone()])
                        .with_ttl_ms(0)
                        .with_cache_scope(CacheScope::Public),
                ),
                "resources/list" => encode(
                    ListResourcesResult::with_all_items(vec![skill.resource()])
                        .with_ttl_ms(0)
                        .with_cache_scope(CacheScope::Public),
                ),
                "resources/templates/list" => encode(
                    ListResourceTemplatesResult::with_all_items(vec![])
                        .with_ttl_ms(0)
                        .with_cache_scope(CacheScope::Public),
                ),
                _ => Err(ErrorData::invalid_params("Unknown skill request.", None)),
            }
        }
    }
}

fn parse<'de, T: Deserialize<'de>>(params: &'de Value) -> Result<T, ErrorData> {
    T::deserialize(params).map_err(|_| ErrorData::invalid_params("Invalid skill arguments.", None))
}

fn encode(result: impl Serialize) -> Result<Value, ErrorData> {
    serde_json::to_value(result)
        .map_err(|_| ErrorData::internal_error("Cannot serialize the Beankeeper skill.", None))
}
